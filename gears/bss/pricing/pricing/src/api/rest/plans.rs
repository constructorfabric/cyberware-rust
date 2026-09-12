//! The plan authoring plane: `GET`, `POST`, `PATCH` and `POST …/abandon`
//! (`design/02-plan-definition.md` §5, D-140, D-141, D-145, D-146).
//!
//! Every route here gates on `plan x write` - or `plan x read` for the read  -
//! **before** it touches a repository (`01-foundation.md` §2.2), and the pair is
//! taken from [`crate::authz`] rather than spelled here, so a route gated on the
//! wrong action is a test failure rather than a reading of source order.
//!
//! # What this surface validates, and what it deliberately does not
//!
//! Nothing here judges a plan's **shape**. The billing-cycle matrix, the phase
//! graph, the add-on rules and the descriptor set are all validated at
//! **publish** (§4.2 step 2), and authoring an incomplete draft is legal by
//! design - an author assembles a plan over several calls, and refusing an
//! intermediate state would make the pre-check-at-publish design unreachable. So
//! a `PATCH` that leaves a plan with no `billing_cycle` succeeds, and the plan
//! is simply not publishable yet.
//!
//! # The refusals this plane can raise, all of them Foundation-owned
//!
//! `STALE_VERSION` (409), `LIFECYCLE_FORBIDDEN` (400 carrying its code),
//! `OPEN_DRAFT_REVISION_EXISTS` (409), `PLAN_RETIRED_NO_SUCCESSOR` (400),
//! `PLAN_ABANDONED_NO_SUCCESSOR` (400), `IDEMPOTENCY_PAYLOAD_MISMATCH` (409),
//! `IDEMPOTENCY_KEY_IN_FLIGHT` (409). The design set calls several of those 422s
//! and they arrive as **400s carrying their code** - §3.3 states it once for the
//! whole set: the platform has no 422 category, and the code string is the
//! discriminator a consumer matches on, not the status. No route here declares a
//! 422 response.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Extension, Path, Query};
use axum::http::header::{ETAG, LOCATION};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, http::HeaderMap, http::StatusCode};
use bss_pricing_sdk::odata::{PlanFilterField, PlanOrderField};

use toolkit::api::canonical_prelude::CanonicalError;
use toolkit::api::odata::OData;
use toolkit::api::operation_builder::{OperationBuilderODataExt, ParamLocation, ParamSpec};
use toolkit::api::{OpenApiRegistry, operation_builder::OperationBuilder};
use toolkit_db::secure::{AccessScope, DbTx};
use toolkit_odata::Page;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::auth_context::{audit_stamp, require_authenticated};
use crate::api::rest::correlation::{CorrelationId, require_correlation};
use crate::api::rest::error::authz_error_to_canonical;
use crate::api::rest::odata_list::{map_odata_page_err, reject_non_odata_list_params};
use crate::api::rest::preconditions::{self, RevisionTag};
use crate::api::rest::state::AuthoringState;
use crate::domain::audit::AuditStamp;
use crate::domain::concurrency::RowVersion;
use crate::domain::contracts::{
    EntitlementGrants, GrantSet, PlanChangeContract, UsageCounterOnPlanChange,
};
use crate::domain::error::DomainError;
use crate::domain::instant::format_rfc3339;
use crate::domain::instant::rfc3339;
use crate::domain::lifecycle::LifecycleState;
use crate::domain::money::{CurrencyCode, MinorAmount};
use crate::domain::plan::{PlanRevision, PlanShapePatch};
use crate::domain::plan_rules::composition::AddonQtyRange;
use crate::domain::plan_rules::cycle_shape::PurchaseQtyRange;
use crate::domain::plan_rules::period_floor_cap::PeriodFloorCapAmounts;
use crate::domain::plan_shape::{
    AddonRule, BillingCycle, CompositeMeter, CustomIntervalUnit, DescriptorSet, Frequency,
    PeriodFloorCap, PhaseKind, PlanPhase, PlanShape,
};
use crate::domain::scope_key::{PhaseId, PlanId, Region};
use crate::domain::validation::ValidationPipeline;
use crate::infra::clone::{CloneNotice, CloneReceipt, SeededPhaseOrigin, clone_plan_on};
use crate::infra::idempotent::{self, Guarded, GuardedRequest, TxFuture};
use crate::infra::storage::repo::{NewPlanDraft, PlanRepo, plan_repo, plan_shape_repo};
use crate::infra::storage::{RepoError, repo_failure};
use time::OffsetDateTime;

/// `OpenAPI` tag applied to every plan operation (DE0205).
const TAG: &str = "BSS Pricing Plans";

/// The plan collection.
///
/// The literal is repeated in the `OperationBuilder` call below because DE0801
/// validates a **literal** argument and silently passes a `const` one - so the
/// route shape rule only binds where the literal is. The two spellings are
/// pinned together by
/// `module_test::the_registered_route_set_is_exactly_the_declared_paths`, which
/// is what keeps the second spelling from drifting.
pub const PLANS: &str = "/bss-pricing/v1/plans";
/// One plan, by id.
pub const PLAN: &str = "/bss-pricing/v1/plans/{planId}";
/// The plan counts by authoring state (D-360). A static segment beside `{planId}`:
/// Axum matches it first, and a plan id is a UUID, so the two cannot collide.
pub const PLANS_COUNTS: &str = "/bss-pricing/v1/plans/counts";
/// The abandon action, as a sub-resource segment (D-140: never a colon method).
pub const PLAN_ABANDON: &str = "/bss-pricing/v1/plans/{planId}/abandon";

/// `POST` — clone a plan into a new draft plan (§5, `algo-clone`; D-19, D-275).
///
/// A `plans` route rather than a module of its own, and the reason is the path:
/// the source is a plan and the surface belongs to the plan family, exactly as
/// `/abandon` does. A separate module would owe a router, a mount, a merge and
/// two censuses to say the same thing.
pub const PLAN_CLONE: &str = "/bss-pricing/v1/plans/{planId}/clone";

/// §5's wire code for a clone whose source has nothing published.
///
/// Beside the route rather than in the domain, `preview::PRICE_ROW_ABSENT`'s
/// placement exactly: a wire code belongs to the surface that returns it, and
/// the surface is what the design set's Problem-responses block is about.
pub const CLONE_SOURCE_NOT_FOUND: &str = "CLONE_SOURCE_NOT_FOUND";

/// The `If-None-Match` header, declared on every read that honours it.
///
/// [`if_match_param`]'s reason on the read side, and it is the weaker of the two
/// claims deliberately: this header is **optional**, so a client that never sends
/// it is served the representation. What the declaration buys is that a client
/// generated from this spec knows the round trip is available at all — the gear
/// emits a validator on seven reads, and a read that emits one without honouring
/// this header makes a caller polling for a fresh precondition re-download the whole
/// representation every time to obtain one.
pub(crate) fn if_none_match_param() -> ParamSpec {
    ParamSpec {
        name: "If-None-Match".to_owned(),
        location: ParamLocation::Header,
        required: false,
        description: Some(
            "Optional conditional-read precondition (RFC 9110). Send the `ETag` this route last \
             answered; if it still names the current representation the answer is `304` with the \
             tag and no body. The wildcard `*` and a comma-separated list are both accepted, and \
             comparison is weak, per RFC 9110 section 13.1.2 - deliberately laxer than this \
             gear's `If-Match`, where a wildcard would be an unconditional write."
                .to_owned(),
        ),
        param_type: "string".to_owned(),
        // Scalar: every parameter this gear declares is single-valued.
        // `array` arrived upstream for `?tag=a&tag=b` repeats, which no route
        // here has.
        array: false,
    }
}

/// The `If-Match` header, declared so a generated client knows it is mandatory.
///
/// `OperationBuilder` has `path_param` and `query_param` but no header builder,
/// so the spec is written out — the sibling credstore surface does the same at
/// `api/rest/routes.rs`. **Declaring it is not decoration**: without it a client
/// generated from this spec cannot satisfy the precondition on any mutating
/// verb, and learns of it from a 400. The same discipline this gear already
/// applies to responses, applied to requests.
pub(crate) fn if_match_param(subject: &str) -> ParamSpec {
    ParamSpec {
        name: "If-Match".to_owned(),
        location: ParamLocation::Header,
        required: true,
        description: Some(format!(
            "Mandatory optimistic-concurrency precondition (RFC 9110), and D-141's rule that \
             every mutating verb on a draft presents its `ETag`. {subject} A mismatch is `409` \
             `STALE_VERSION`; an absent or malformed header is `400`. Weak validators, the \
             wildcard `*` and tag lists are all refused - a wildcard is an unconditional \
             mutation, which is what the precondition exists to prevent."
        )),
        param_type: "string".to_owned(),
        // Scalar: every parameter this gear declares is single-valued.
        // `array` arrived upstream for `?tag=a&tag=b` repeats, which no route
        // here has.
        array: false,
    }
}

/// The `Idempotency-Key` header, likewise.
pub(crate) fn idempotency_key_param() -> ParamSpec {
    ParamSpec {
        name: "Idempotency-Key".to_owned(),
        location: ParamLocation::Header,
        required: true,
        description: Some(
            "Mandatory client idempotency key (S2/S3 API surface, foundation step 1). Bounded \
             at 255 printable-ASCII characters. The key is claimed in the same transaction as \
             the mutation it guards, so a retry carrying the same key and body is answered the \
             recorded response verbatim, and one carrying the same key with a different body \
             is `409` `IDEMPOTENCY_PAYLOAD_MISMATCH`. An absent header is `400`: an unguarded \
             create on a governed authoring plane is the retry hazard the gate exists for."
                .to_owned(),
        ),
        param_type: "string".to_owned(),
        // Scalar: every parameter this gear declares is single-valued.
        // `array` arrived upstream for `?tag=a&tag=b` repeats, which no route
        // here has.
        array: false,
    }
}

// ---------------------------------------------------------------------------
// Views. `*View` rather than `*Dto`, carrying `api_dto` anyway (DE0203/DE0204
// do not reach the name, and the macro is what fixes the wire shape to
// snake_case and registers the schema).
// ---------------------------------------------------------------------------

/// A recurring frequency, with the custom interval that rides its variant.
///
/// One object rather than three sibling members, because the domain type
/// [`Frequency`] cannot represent a fixed frequency carrying an interval and the
/// wire must not be able to either: three flat members would let a caller send
/// `monthly` with an interval, which is the pairing
/// `chk_pricing_plan_custom_interval_pairing` exists to refuse.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct FrequencyView {
    /// The frequency token (`monthly`, `quarterly`, `semiannual`, `annual`,
    /// `custom_every_n`).
    pub kind: String,
    /// The interval count, set on `custom_every_n` and on nothing else.
    pub custom_interval_n: Option<u32>,
    /// The interval unit (`days`, `months`), likewise.
    pub custom_interval_unit: Option<String>,
}

impl From<Frequency> for FrequencyView {
    fn from(frequency: Frequency) -> Self {
        let (n, unit) = match frequency {
            Frequency::CustomEveryN { n, unit } => (Some(n), Some(unit.as_str().to_owned())),
            Frequency::Monthly
            | Frequency::Quarterly
            | Frequency::Semiannual
            | Frequency::Annual => (None, None),
        };
        Self {
            kind: frequency.as_str().to_owned(),
            custom_interval_n: n,
            custom_interval_unit: unit,
        }
    }
}

/// One phase of a revision's phase chain.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct PlanPhaseView {
    /// Stable across revisions (D-83): the `phase` scope-key axis is filed under
    /// it, so a copy-forward keeps the id rather than minting a new one.
    pub phase_id: Uuid,
    /// `trial` | `interim` | `evergreen`. `intro` is not accepted as a legacy
    /// spelling (D-358).
    pub kind: String,
    /// The operator's label for the phase (D-357): optional and free-form. Absent
    /// in a body means "no label"; a blank string is read as absent.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Position in the chain.
    pub ordinal: i32,
    /// The phase this one converts into; `null` on the terminal phase.
    pub converts_to_phase_id: Option<Uuid>,
    /// How long the phase lasts; `null` on the terminal phase.
    pub phase_duration_days: Option<u32>,
    /// The trial length a storefront displays, on a `trial` phase.
    pub display_trial_days: Option<u32>,
}

impl From<PlanPhase> for PlanPhaseView {
    fn from(phase: PlanPhase) -> Self {
        Self {
            phase_id: phase.phase_id.get(),
            kind: phase.kind.as_str().to_owned(),
            display_name: phase.display_name,
            ordinal: phase.ordinal,
            converts_to_phase_id: phase
                .converts_to_phase_id
                .map(crate::domain::scope_key::PhaseId::get),
            phase_duration_days: phase.phase_duration_days,
            display_trial_days: phase.display_trial_days,
        }
    }
}

/// One add-on composition rule.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct AddonRuleView {
    /// The add-on SKU this rule is about.
    pub addon_sku_id: Uuid,
    /// Whether the add-on must be taken.
    pub required: bool,
    /// Selection-time lower bound.
    pub min_qty: Option<u32>,
    /// Selection-time upper bound.
    pub max_qty: Option<u32>,
    /// Selection-time quantity step.
    pub step_qty: Option<u32>,
    /// An alternative price for this add-on when taken with this plan.
    pub price_override_ref: Option<Uuid>,
    /// Add-on SKU ids this one requires.
    pub depends_on: Vec<Uuid>,
    /// Add-on SKU ids this one excludes.
    pub conflicts_with: Vec<Uuid>,
}

impl From<AddonRule> for AddonRuleView {
    fn from(rule: AddonRule) -> Self {
        Self {
            addon_sku_id: rule.addon_sku_id,
            required: rule.required,
            min_qty: rule.min_qty,
            max_qty: rule.max_qty,
            step_qty: rule.step_qty,
            price_override_ref: rule.price_override_ref,
            depends_on: rule.depends_on,
            conflicts_with: rule.conflicts_with,
        }
    }
}

/// One market's plan-level period floor and cap (S2 §6, **D-319**).
///
/// The currency is the market axis **and** the denomination of both amounts:
/// there is no `currency` field on the money, because a second spelling of the
/// denomination is a second thing to disagree.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct PeriodFloorCapView {
    /// ISO 4217.
    pub currency: String,
    /// The market's region axis value.
    pub region: String,
    /// The period floor in minor units, or `null` when only a cap is authored.
    pub floor_minor: Option<i64>,
    /// The period cap in minor units, or `null` when only a floor is authored.
    pub cap_minor: Option<i64>,
}

impl From<PeriodFloorCap> for PeriodFloorCapView {
    fn from(bound: PeriodFloorCap) -> Self {
        Self {
            currency: bound.currency.as_str().to_owned(),
            region: bound.region.as_str().to_owned(),
            floor_minor: bound.floor_minor.map(MinorAmount::get),
            cap_minor: bound.cap_minor.map(MinorAmount::get),
        }
    }
}

/// The revision's billing descriptor set.
///
/// Its **absence** is `null` on [`PlanView::descriptor_set`], and an attached
/// set whose three named members are all empty is an object with three nulls.
/// The distinction is kept on the wire because the store keeps it - an
/// unattached set has no row at all - and because `DESCRIPTOR_INCOMPLETE` is
/// asked of an attached set: collapsing the two would make "nobody attached one"
/// and "somebody attached an empty one" the same publish input.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct DescriptorSetView {
    /// The invoice line template Billing renders from.
    pub invoice_line_template: Option<String>,
    /// The general-ledger code the posting lands on.
    pub gl_code: Option<String>,
    /// How the plan's charges are composed into invoice lines.
    pub itemization_rule: Option<String>,
    /// Extra descriptor keys a deployment's required-set names (P5).
    ///
    /// A `BTreeMap` rather than a `HashMap`, and the reason is the idempotency
    /// digest: `serde_json` renders a `BTreeMap` in key order and a `HashMap` in
    /// whatever order the process happens to hash to, so a retry would digest
    /// differently from its own first attempt (see `api::rest::preconditions`).
    pub additional: std::collections::BTreeMap<String, String>,
}

impl From<DescriptorSet> for DescriptorSetView {
    fn from(set: DescriptorSet) -> Self {
        Self {
            invoice_line_template: set.invoice_line_template,
            gl_code: set.gl_code,
            itemization_rule: set.itemization_rule,
            additional: set.additional,
        }
    }
}

/// One derived (composite) meter, as an **author** states it (Slice 10 §6, A4,
/// D-32, D-106).
///
/// # Why this is a twin of `approvals::CompositeMeterView` and not that type
///
/// A composite meter already has a wire shape:
/// [`CompositeMeterView`](crate::api::rest::approvals::CompositeMeterView), minted
/// when the reviewer's pinned document was the **only** surface that had ever
/// shown one. Sharing it was considered and rejected, and the reasons are three,
/// in increasing order of what it costs to get wrong.
///
/// 1. **Direction.** `approvals.rs` imports [`PlanPhaseView`], [`AddonRuleView`],
///    [`DescriptorSetView`] and [`FrequencyView`] *from here*: the authoring plane
///    owns the shapes an author writes, and the reviewer's document borrows them.
///    Making [`PatchPlanRequest`] depend on a type in `approvals.rs` inverts that
///    for one facet, and a reader would then have to know which of the six is the
///    exception.
/// 2. **The two shapes are already answers to different questions.** This module's
///    own Slice-6 pairs are the precedent: [`PlanChangeContractRequest`] beside
///    `approvals::PlanChangeContractView`, [`EntitlementGrantsRequest`] beside
///    `approvals::EntitlementGrantsView` - twins rather than one type, because a
///    request's members carry an *omitted-means-something* reading while a view
///    renders the resolved value (`EntitlementGrantsView::per_phase` says so in as
///    many words: "the **authored** ones, not the materialized map").
///    `PinnedContentView`'s stated invariant is "exactly the fields the pin hashes
///    and no others", which is a rule about the digest and not about what an author
///    may set.
/// 3. **That invariant is live, and it has already moved once.**
///    `PinnedContentView` gained `windows` when the pin started framing them, and a
///    window is not authored on this route at all. Under a shared type that
///    widening would have made a derived, publish-owned fact settable by a `PATCH`,
///    with nothing to notice it: no DTO in this gear sets `deny_unknown_fields`, so
///    the compiler would have had no complaint and the surface no refusal.
///
/// What the twin costs is one field that differs and three that do not, joined in
/// exactly one place ([`composite_of`]).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct CompositeMeterRequest {
    /// The definition's identity, **stable across revisions** (D-106).
    ///
    /// Optional, which is the one member that diverges from the reviewer's view
    /// (where it is a plain `Uuid`, because a reviewer is only ever shown a
    /// persisted row). Two readings, and both are needed:
    ///
    /// - **Omitted: author a new definition.** The surface mints the id, as
    ///   `POST /plans` mints a `planId` and for a sharper version of the same
    ///   reason. Until `pricing_composite_meter`, `pricing_composite_meter`'s
    ///   `PRIMARY KEY (composite_id, plan_revision)` carried neither `plan_id` nor
    ///   `tenant_id`, so it was `pricing_plan_phase`'s collision one table over -
    ///   two plans of one tenant standing at revision `0` could not share a child
    ///   id, and `rest_plans.rs` pinned that as a client-reachable `500`. The key
    ///   now carries both, so the collision is gone in **both** directions the
    ///   optionality exposed; a **required** id would still have been the wrong
    ///   call, for the reading below rather than for a defect.
    /// - **Supplied: keep this definition.** A `GET` echoes the ids, so a
    ///   read-modify-write round trip preserves identity through a facet that
    ///   replaces wholesale - which is what makes D-106's "a draft's formula edit
    ///   leaves the published revision byte-identical" true of an *edit* and not
    ///   only of the copy-forward. That echo is why [`PlanView`] gains this facet in
    ///   the same change the `PATCH` does.
    pub composite_id: Option<Uuid>,
    /// The registry-declared unit this composite rates as (`inst-cm-output`).
    pub output_unit: String,
    /// The constituent unit ids. Two or more **distinct** ids is
    /// [`CompositeArity`](crate::domain::plan_rules::composite::CompositeArity)'s
    /// rule and is judged at publish; whether each is *published* is unasked,
    /// because this gear holds no registry client.
    pub constituent_units: Vec<String>,
    /// The formula as data, verbatim as authored.
    ///
    /// Opaque on purpose (A4): the catalog persists and freezes it and Rating
    /// computes from the snapshot, so nothing in this crate reads inside it and
    /// nothing here can refuse its contents. Omitted stores JSON `null`, which is
    /// an author who stated no formula rather than a missing field - the design set
    /// declares no code that judges it either way, and inventing one here would be
    /// minting a refusal no document names.
    pub formula: Option<serde_json::Value>,
}

impl From<CompositeMeter> for CompositeMeterRequest {
    fn from(meter: CompositeMeter) -> Self {
        Self {
            // `Some`, always: a read answers persisted rows, and a `null` id on the
            // way out would invite a round trip to re-mint the very identity D-106
            // keeps stable.
            composite_id: Some(meter.composite_id),
            output_unit: meter.output_unit,
            constituent_units: meter.constituent_units,
            formula: Some(meter.formula),
        }
    }
}

/// One plan revision, whole: its own columns and its five child sets.
///
/// The child sets are here rather than on sub-resources of their own because
/// S2 §5's `PATCH` cell names those facets as the plan's shape - a read
/// that omitted them could not round-trip a patch, and an author would have to
/// guess what a `PATCH` was about to replace.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PlanView {
    /// The plan this revision belongs to.
    pub plan_id: Uuid,
    /// The revision number. An **identity, not a counter** (D-145): the sequence
    /// may have gaps where a draft was abandoned.
    pub revision: u64,
    /// `draft` | `abandoned` | `published` | `superseded` | `retired`. Named on
    /// the wire so a caller never has to infer which revision it was given.
    pub lifecycle_state: String,
    /// The catalog SKU this plan realizes, when one is bound.
    pub sku_id: Option<Uuid>,
    /// The plan's tier (a registry-owned taxonomy, so a string).
    pub plan_tier: Option<String>,
    /// The plan's human label (D-318), absent until an operator names it.
    pub plan_name: Option<String>,
    /// `one_time` | `recurring` | `usage` | `hybrid`.
    pub billing_cycle: Option<String>,
    /// The recurring frequency, interval and all.
    pub frequency: Option<FrequencyView>,
    /// Whether the tier diverges from the parent SKU's under an audited
    /// override.
    pub plan_tier_override: bool,
    /// Minimum purchasable quantity (one-time plans).
    pub purchase_min_qty: Option<u64>,
    /// Maximum purchasable quantity (one-time plans).
    pub purchase_max_qty: Option<u64>,
    /// The Billing invoice-layout hint (D-96).
    pub invoice_grouping_key: Option<String>,
    /// Start of the availability window, UTC.
    #[serde(default, with = "rfc3339::option")]
    pub available_from: Option<OffsetDateTime>,
    /// End of the availability window, UTC.
    #[serde(default, with = "rfc3339::option")]
    pub available_to: Option<OffsetDateTime>,
    /// Pseudonymous principal id of the authoring actor.
    pub created_by: Uuid,
    /// When the plan's first revision was created, UTC; stable across revisions.
    #[serde(with = "rfc3339")]
    pub created_at: OffsetDateTime,
    /// When this shown revision was created, UTC.
    #[serde(with = "rfc3339")]
    pub revision_created_at: OffsetDateTime,
    /// The revision's optimistic-concurrency version - the same number the
    /// `ETag` header quotes, carried in the body too so a client that cannot see
    /// response headers can still submit a precondition.
    pub row_version: u64,
    /// The revision's phase chain, in the order the store returns.
    pub phases: Vec<PlanPhaseView>,
    /// The revision's add-on composition rules.
    pub addon_rules: Vec<AddonRuleView>,
    /// The revision's descriptor set, or `null` when none is attached.
    pub descriptor_set: Option<DescriptorSetView>,
    /// The revision's derived (composite) meter set, in the store's order.
    ///
    /// **A write surface whose read surface does not show the value is half a
    /// feature**, and here it is load-bearing rather than symmetric-for-tidiness:
    /// the `composites` facet replaces the set wholesale, and
    /// [`CompositeMeterRequest::composite_id`] is what preserves a definition's
    /// identity across such a replace. An author with no way to read the ids back
    /// could only ever re-mint them, and D-106's stable id would hold for the
    /// copy-forward and break for every edit.
    ///
    /// An empty list is a revision that defines no composite, which is the ordinary
    /// case: unlike [`PlanView::descriptor_set`] there is no attached-but-empty
    /// state to keep apart from absence, because the set is rows and not a row.
    pub composites: Vec<CompositeMeterRequest>,
    /// The revision's period floor/cap set, in `(currency, region)` order
    /// (**D-319**).
    ///
    /// A write surface whose read surface does not show the value is half a
    /// feature, and here the read is what makes a wholesale replace usable at
    /// all: the facet replaces the set, so an author adding a bound in a second
    /// market has to be able to read the first one back to resubmit it.
    ///
    /// An empty list is a plan with no minimum, which is the ordinary plan.
    pub period_floor_caps: Vec<PeriodFloorCapView>,
    /// The revision's entitlement grant set (Slice 6, §6, D-41).
    ///
    /// A write surface whose read surface does not show the value is half a
    /// feature, for the third time on this view and with the sharpest version of
    /// the argument: the `shape` facet's `entitlement_grants` member is **sent
    /// whole or not at all**, so an author adding one quota has to resubmit the
    /// flags, the tier reference and every per-phase set beside it. Without a
    /// surface that answers them the
    /// `PATCH` accepted the set, the create stored it, the pin hashed it and
    /// `approvals::PinnedContentView` showed it to a *reviewer*, and the author
    /// who wrote it could not read it back.
    ///
    /// It is the [`EntitlementGrantsRequest`] twin and not
    /// `approvals::EntitlementGrantsView`, which is the split the module doc
    /// states: the reviewer's view renders the **resolved** per-phase map, and
    /// this is the authoring plane, where what an author needs answered is what
    /// they may send back.
    pub entitlement_grants: EntitlementGrantsRequest,
    /// The revision's plan-change contract (Slice 6, §6).
    ///
    /// Here for [`PlanView::entitlement_grants`]' reason, and it carries one
    /// distinction the round trip depends on: `allowed_change_targets` is `null`
    /// for the fail-safe and `[]` for an author who cleared their edges, and the
    /// two are different states of the same plan.
    pub change_contract: PlanChangeContractRequest,
    /// Submitted approvals belonging directly to this plan: revision, window
    /// and price units. Empty when none are pending; sorted by submission time
    /// and approval id. Matches the list contract.
    ///
    /// **Derived, not stored.** No plan state says "in review": the revision's
    /// lifecycle does not change just because a unit opens. This field is read
    /// from the approval store on every GET, following `voided`,
    /// `rejected`, `withdrawn` and `approved` without a second writer of one
    /// fact. It is **not part of the `ETag`**, which digests the revision and its
    /// `row_version`: a unit opening or closing is not a change to plan content,
    /// and a tag that moved with it would stale every author's `If-Match` for
    /// something they did not edit.
    ///
    /// GET always returns a fresh body with `Cache-Control: private, no-store`;
    /// it does not evaluate `If-None-Match`. The tag is a write precondition,
    /// not a validator for these derived fields. Advisory, like every snapshot:
    /// the unit can be decided between this read
    /// and the caller's next act, and the write path stays the authority
    /// (`PENDING_CHANGE_UNIT_EXISTS` on a second submit,
    /// `APPROVAL_NOT_PENDING` on a stale decision).
    pub pending_approvals: Vec<PendingApprovalView>,
}

/// One submitted approval belonging directly to a plan's aggregate.
///
/// Only what a `plan × read` caller may learn: **that** a unit is pending,
/// **which** one, and since when. The submitter, the verdict, the reason and the
/// pinned content sit behind `approval × read` on
/// `GET /bss-pricing/v1/approvals/{approvalId}`, and this view exists so a
/// client can open that resource under its own gate rather than page the
/// approvals list and join on `subject_ref` — which it cannot narrow
/// server-side today.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PendingApprovalView {
    /// The unit to open under `approval × read`.
    pub approval_id: Uuid,
    /// `plan_revision`, `window` or `price_unit`.
    pub subject_kind: String,
    /// When the unit was opened, UTC.
    #[serde(with = "rfc3339")]
    pub submitted_at: OffsetDateTime,
}

impl PlanView {
    /// Compose a revision with its five child sets.
    fn new(
        revision: PlanRevision,
        created_at: OffsetDateTime,
        phases: Vec<PlanPhase>,
        addon_rules: Vec<AddonRule>,
        descriptor_set: Option<DescriptorSet>,
        composites: Vec<CompositeMeter>,
        period_floor_caps: Vec<PeriodFloorCap>,
    ) -> Self {
        Self {
            plan_id: revision.plan_id.get(),
            revision: revision.revision,
            lifecycle_state: revision.lifecycle_state.as_str().to_owned(),
            sku_id: revision.sku_id,
            plan_tier: revision.plan_tier,
            plan_name: revision.plan_name,
            billing_cycle: revision
                .billing_cycle
                .map(|cycle| cycle.as_str().to_owned()),
            frequency: revision.frequency.map(FrequencyView::from),
            plan_tier_override: revision.plan_tier_override,
            purchase_min_qty: revision.purchase_min_qty,
            purchase_max_qty: revision.purchase_max_qty,
            invoice_grouping_key: revision.invoice_grouping_key,
            available_from: revision.available_from,
            available_to: revision.available_to,
            created_by: revision.created_by,
            created_at,
            revision_created_at: revision.created_at_utc,
            row_version: revision.row_version.get(),
            phases: phases.into_iter().map(PlanPhaseView::from).collect(),
            addon_rules: addon_rules.into_iter().map(AddonRuleView::from).collect(),
            descriptor_set: descriptor_set.map(DescriptorSetView::from),
            composites: composites
                .into_iter()
                .map(CompositeMeterRequest::from)
                .collect(),
            period_floor_caps: period_floor_caps
                .into_iter()
                .map(PeriodFloorCapView::from)
                .collect(),
            // Both are columns of the revision rather than child sets, so neither
            // costs a query: `plan_repo::to_entitlement_grants` and its change-contract
            // pair already decoded them on the read that produced `revision`.
            entitlement_grants: EntitlementGrantsRequest::from(revision.entitlement_grants),
            change_contract: PlanChangeContractRequest::from(revision.change_contract),
            // Filled by the read handler, which is the only caller that has a
            // connection to ask the approval store with (D-359).
            pending_approvals: Vec::new(),
        }
    }
}

/// One plan on a page: the revision an author is holding, and nothing that costs
/// a query of its own.
///
/// The five child sets [`PlanView`] carries are each a query of their own, so
/// rendering them a hundred times for a catalogue screen would be six hundred
/// round trips to show a tier and a state. A caller opens
/// `GET …/plans/{planId}` for the rest — the split
/// `list_approvals` / `get_approval` already make one module over.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PlanSummaryView {
    /// The plan this revision belongs to.
    pub plan_id: Uuid,
    /// The revision number the page is answering about — the open draft's when
    /// the plan has one, else the current revision's.
    pub revision: u64,
    /// `draft` | `abandoned` | `published` | `superseded` | `retired`.
    pub lifecycle_state: String,
    /// The catalog SKU this plan realizes, when one is bound.
    pub sku_id: Option<Uuid>,
    /// The plan's tier (a registry-owned taxonomy, so a string).
    pub plan_tier: Option<String>,
    /// The plan's human label (D-318), absent until an operator names it.
    pub plan_name: Option<String>,
    /// `one_time` | `recurring` | `usage` | `hybrid`.
    pub billing_cycle: Option<String>,
    /// Start of the availability window, UTC.
    #[serde(default, with = "rfc3339::option")]
    pub available_from: Option<OffsetDateTime>,
    /// End of the availability window, UTC.
    #[serde(default, with = "rfc3339::option")]
    pub available_to: Option<OffsetDateTime>,
    /// When the plan's first revision was created, UTC; stable across revisions.
    #[serde(with = "rfc3339")]
    pub created_at: OffsetDateTime,
    /// When this shown revision was created, UTC.
    #[serde(with = "rfc3339")]
    pub revision_created_at: OffsetDateTime,
    /// The precondition a caller needs to `PATCH` this row without reading it
    /// again. Carried on the page for the same reason [`PlanView`] carries it: a
    /// client that cannot see response headers can still submit an `If-Match`.
    pub row_version: u64,
    /// Same pending metadata as [`PlanView::pending_approvals`]. Empty when no
    /// directly owned unit is submitted; oldest first, then approval id.
    /// Computed and advisory; filter its non-emptiness with `has_pending_approvals`.
    pub pending_approvals: Vec<PendingApprovalView>,
    /// The plan's `draft` + `published` rows — the count `GET …/prices` returns
    /// with no `$filter` (**D-360**). Filled by `list_plans` from one grouped
    /// read over the page's plans, so a catalogue screen no longer reads every
    /// plan's rows to show it.
    pub price_row_count: u64,
    /// Distinct `model_kind`s of those rows, sorted; a row with none contributes
    /// no kind (D-360).
    pub model_kinds: Vec<String>,
    /// Distinct currencies of those rows, sorted (D-360).
    pub currencies: Vec<String>,
}

/// The tenant's plans counted by **authoring** state — what the list's rows
/// show, over the whole catalogue rather than one page (D-360). `total` is the
/// number of plans; the three states sum to it.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PlanCountsView {
    /// The number of plans in the authoring set.
    pub total: u64,
    /// Plans holding an open draft, whatever their current revision's state — the
    /// list's own collapse.
    pub draft: u64,
    /// Plans whose current revision is `published` and that hold no draft.
    pub published: u64,
    /// Plans whose current revision is `retired` and that hold no draft.
    pub retired: u64,
}

impl From<&plan_repo::PlanListEntry> for PlanSummaryView {
    fn from(entry: &plan_repo::PlanListEntry) -> Self {
        let revision = &entry.revision;
        Self {
            plan_id: revision.plan_id.get(),
            revision: revision.revision,
            lifecycle_state: revision.lifecycle_state.as_str().to_owned(),
            sku_id: revision.sku_id,
            plan_tier: revision.plan_tier.clone(),
            plan_name: revision.plan_name.clone(),
            billing_cycle: revision
                .billing_cycle
                .map(|cycle| cycle.as_str().to_owned()),
            available_from: revision.available_from,
            available_to: revision.available_to,
            created_at: entry.created_at,
            revision_created_at: revision.created_at_utc,
            row_version: revision.row_version.get(),
            // The revision alone cannot answer this; `list_plans` fills it from
            // one read of the tenant's pending units (D-359).
            pending_approvals: Vec::new(),
            // Nor these: `list_plans` fills them from one grouped read over the
            // page's plans (D-360). A plan with no authoring row keeps them.
            price_row_count: 0,
            model_kinds: Vec::new(),
            currencies: Vec::new(),
        }
    }
}

/// The plan shape a create authors, and the one facet a `PATCH` may move.
///
/// Every member is optional because a plan is assembled over several calls: the
/// shape rules run at **publish**, not at save, so an unfinished draft is a
/// legal draft (§4.2 step 2).
#[derive(Debug, Clone, Default)]
#[toolkit_macros::api_dto(request, response)]
pub struct PlanShapeRequest {
    /// Bind the plan to a catalog SKU.
    pub sku_id: Option<Uuid>,
    /// The plan's tier (a registry-owned taxonomy, so a free string).
    pub plan_tier: Option<String>,
    /// The plan's human label (D-318). Free text an operator chose, distinct
    /// from the tier, which is a classification the catalog reasons about.
    ///
    /// Absent leaves it alone; the empty string is **refused**, not stored, so
    /// `NULL` stays the only spelling of "unnamed".
    pub plan_name: Option<String>,
    /// `one_time` | `recurring` | `usage` | `hybrid`.
    pub billing_cycle: Option<String>,
    /// The recurring frequency, interval and all.
    pub frequency: Option<FrequencyView>,
    /// Declare or withdraw the audited tier override (P3).
    pub plan_tier_override: Option<bool>,
    /// Minimum purchasable quantity (one-time plans).
    pub purchase_min_qty: Option<u64>,
    /// Maximum purchasable quantity (one-time plans).
    pub purchase_max_qty: Option<u64>,
    /// The Billing invoice-layout hint (D-96).
    pub invoice_grouping_key: Option<String>,
    /// Start of the availability window, UTC.
    #[serde(default, with = "rfc3339::option")]
    pub available_from: Option<OffsetDateTime>,
    /// End of the availability window, UTC.
    #[serde(default, with = "rfc3339::option")]
    pub available_to: Option<OffsetDateTime>,
    /// The entitlement grant set (Slice 6, §6, D-41): the plan-level feature
    /// flags and quotas, the `PlanTier` they resolved from when they did, and
    /// any per-phase sets keyed by `phaseId`.
    ///
    /// Sent **whole or not at all**, for the change contract's reason below.
    pub entitlement_grants: Option<EntitlementGrantsRequest>,
    /// The plan-change contract (Slice 6, §6): the published `planId`s a
    /// self-service change may travel to, the comparability rank that
    /// classifies one, and D-113's tier-`Q` continuity flag.
    ///
    /// Sent **whole or not at all**, which is the shape
    /// [`PlanShapePatch::change_contract`] gives it: K4 ties the rank to whether
    /// the edge list names anyone, so a caller able to move one member alone
    /// could express a state no publish accepts.
    pub change_contract: Option<PlanChangeContractRequest>,
}

/// The entitlement grant set on the wire.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct EntitlementGrantsRequest {
    /// The `PlanTier` policy this set resolved from, when it did. Carried for
    /// auditability beside the resolved set, never instead of it
    /// (`inst-gs-resolved`).
    pub plan_tier_ref: Option<String>,
    /// Plan-level `featureFlag: bool` entries.
    pub feature_flags: Option<BTreeMap<String, bool>>,
    /// Plan-level `quotaKey: value` entries.
    pub quotas: Option<BTreeMap<String, i64>>,
    /// Optional per-phase sets, keyed by `phaseId` (D-41). Every key is checked
    /// against the plan's own phase schedule at publish, not here.
    pub per_phase: Option<BTreeMap<Uuid, GrantSetRequest>>,
}

/// One grant set on the wire.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct GrantSetRequest {
    /// `featureFlag: bool` entries.
    pub feature_flags: Option<BTreeMap<String, bool>>,
    /// `quotaKey: value` entries.
    pub quotas: Option<BTreeMap<String, i64>>,
}

impl From<GrantSet> for GrantSetRequest {
    fn from(set: GrantSet) -> Self {
        Self {
            // `Some`, always, and an empty map rather than `null`. The two members
            // are `Option` on the way *in* because [`read_grants`] reads an omitted
            // one as "nothing granted"; on the way out there is no such question, and
            // a `null` a client fed straight back would be indistinguishable from the
            // empty set it stands for.
            feature_flags: Some(set.feature_flags),
            quotas: Some(set.quotas),
        }
    }
}

impl From<EntitlementGrants> for EntitlementGrantsRequest {
    fn from(grants: EntitlementGrants) -> Self {
        Self {
            plan_tier_ref: grants.plan_tier_ref,
            feature_flags: Some(grants.plan_level.feature_flags),
            quotas: Some(grants.plan_level.quotas),
            per_phase: Some(
                grants
                    .per_phase
                    .into_iter()
                    .map(|(id, set)| (id, GrantSetRequest::from(set)))
                    .collect(),
            ),
        }
    }
}

/// The plan-change contract on the wire.
///
/// `request, response` rather than `request`, because
/// [`PlanShapeRequest`] is both and a member of a response DTO has to be
/// serializable too.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct PlanChangeContractRequest {
    /// Explicit published `planId`s. **Omit the field entirely** to state the
    /// fail-safe — no self-service change (`inst-pc-failsafe`) — which is not
    /// the same as sending an empty list.
    pub allowed_change_targets: Option<Vec<Uuid>>,
    /// The tenant-wide comparability rank (K4).
    pub comparability_rank: Option<i32>,
    /// `reset` (default) | `carry`. Omitted is `reset`, which is D-113's
    /// ratified default and the safe direction.
    pub usage_counter_on_plan_change: Option<String>,
}

impl From<PlanChangeContract> for PlanChangeContractRequest {
    fn from(contract: PlanChangeContract) -> Self {
        Self {
            // The `Option` is carried **verbatim**, and this is the one member where
            // that matters: `None` is `inst-pc-failsafe` - no self-service change -
            // and `Some([])` is an author who cleared their edges. Collapsing them on
            // the read would make a read-modify-write round trip author the state the
            // plan was not in.
            allowed_change_targets: contract.allowed_change_targets,
            comparability_rank: contract.comparability_rank,
            // Named rather than omitted: absence reads as `reset` on the way in, so a
            // `carry` contract answered as `null` would be resubmitted as `reset` by
            // the round trip this facet exists to support.
            usage_counter_on_plan_change: Some(
                contract.usage_counter_on_plan_change.as_str().to_owned(),
            ),
        }
    }
}

/// A `PATCH` body: **exactly one** facet.
///
/// # Why one and not several
///
/// The reason is structural rather than stylistic.
/// [`PlanShapeRepo::replace_phases`](crate::infra::storage::repo::PlanShapeRepo::replace_phases),
/// `replace_addon_rules`, `set_descriptor_set` and `replace_composites` each
/// compare-and-swap on the **revision's** row version and each bump it - the child
/// sets carry no tag of their own, deliberately, so two authors editing different
/// facets of one draft cannot both satisfy one precondition. A two-facet patch
/// would therefore match the caller's tag on the first mutation and could not
/// match it on the second, and the two are separate transactions with a visible
/// half-applied state in between.
///
/// So more than one facet is [`DomainError::InvalidRequest`] (400, no new code),
/// and **this is a divergence from S2 §5**, whose `PATCH` purpose names several
/// facets in one verb while the storage layer versions them against one tag each
/// of them advances. A coherent multi-facet patch needs a composite operation
/// nobody has designed; it is reported rather than approximated.
///
/// # The fifth facet, and why it is a facet here rather than a route of its own
///
/// `composites` mounts Slice 10's derived-meter set on **this** verb. The set is
/// revision-scoped child rows of a plan revision, versioned by that revision's
/// row version, copied forward by `open_revision` and dropped by `abandon_draft` -
/// which is the description of `phases`, `addon_rules` and `descriptor_set`
/// exactly, and those are facets. A route of its own would owe a second
/// precondition discipline over the same tag, a second idempotency story and a
/// second census, to say what one more arm says here.
///
/// **What it changes is not the surface, it is reachability.** Before it,
/// `replace_composites` had no caller in `src/` at all: the `_on` form was reached
/// only by the clone and `copy_composites` only by `open_revision`, so every
/// production path was a copy of a set nothing could originate. The two registered
/// rules `CompositeArity` and `CompositeSelfReference` therefore ran on every
/// publish over a permanently empty vector - a rule with no operand, which is the
/// defect class D-254 named and D-257 found one field deeper in the same slice.
#[derive(Debug, Clone, Default)]
#[toolkit_macros::api_dto(request, response)]
pub struct PatchPlanRequest {
    /// The plan's own columns.
    pub shape: Option<PlanShapeRequest>,
    /// The whole phase chain, replaced wholesale.
    pub phases: Option<Vec<PlanPhaseView>>,
    /// The whole add-on rule set, replaced wholesale.
    pub addon_rules: Option<Vec<AddonRuleView>>,
    /// The billing descriptor set, attached or replaced.
    pub descriptor_set: Option<DescriptorSetView>,
    /// The whole derived-meter set, replaced wholesale (Slice 10 §6).
    ///
    /// Wholesale like its siblings, and an empty list is the way to withdraw every
    /// composite - the store's own operation is delete-then-insert, so there is no
    /// per-definition verb to expose and no `null`-versus-`[]` distinction to keep.
    pub composites: Option<Vec<CompositeMeterRequest>>,
    /// The whole period floor/cap set, replaced wholesale (S2 §6, **D-319**).
    ///
    /// Wholesale like its siblings, and an empty list is how every bound is
    /// withdrawn: the store's own operation is delete-then-insert, so there is
    /// no per-market verb to expose and no `null`-versus-`[]` distinction to
    /// keep.
    pub period_floor_caps: Option<Vec<PeriodFloorCapView>>,
}

/// Strip the placeholder plan out of a candidate-shape violation's `subject`.
///
/// The three doors below judge a shape the caller has not saved, so they build a
/// [`PlanShape`] around `PlanId::new(Uuid::nil())` for the rule to range over. That
/// placeholder used to reach the caller: `error_mapping` passes a violation's `subject`
/// through to the RFC 9457 `precondition_violation` unchanged, so a 400 for an inverted
/// purchase window arrived naming `00000000-0000-0000-0000-000000000000/0` — a plan that
/// does not exist, while the plan under edit is in the URL. A client branching on
/// `subject`, which is what the design set says the field is for, is routed to the wrong
/// resource.
///
/// The real identity is not available here and threading it would be a wider change
/// than the fault warrants: `shape_of` is the **create** path, where there is no plan
/// yet, and `shape_patch` is reached from a body conversion that holds no id either.
/// So the placeholder is removed rather than replaced. What survives is whatever the
/// rule appended after it — `AddonQtyRange` stamps `{subject}|{addon_sku_id}`, and the
/// SKU is the part that identifies the offending item — and where nothing follows, the
/// subject names the candidate rather than a resource, which is the honest answer.
fn without_placeholder_plan(
    mut report: crate::domain::validation::ValidationReport,
) -> crate::domain::validation::ValidationReport {
    let placeholder =
        PlanShape::new(PlanId::new(Uuid::nil()), 0, OffsetDateTime::now_utc()).subject();
    for violation in &mut report.violations {
        violation.subject = match violation.subject.strip_prefix(&placeholder) {
            Some(rest) => {
                let rest = rest.strip_prefix('|').unwrap_or(rest);
                if rest.is_empty() {
                    "(the candidate shape in this request)".to_owned()
                } else {
                    rest.to_owned()
                }
            }
            None => std::mem::take(&mut violation.subject),
        };
    }
    report
}

/// Refuse an add-on quantity bound the store will refuse anyway (D-150).
///
/// Runs **the same rule** the publish pre-check runs, over a subject carrying
/// nothing but the incoming rules, so there is one spelling of `required =>
/// maxQty >= 1`, `minQty <= maxQty` and `stepQty > 0` rather than a second copy
/// here that drifts from `AddonQtyRange`'s.
///
/// The plan id and revision are placeholders: this rule reads `addon_rules` and
/// nothing else. A real shape would have to be assembled from a read this handler
/// has not done and does not need.
///
/// **The placeholder reaches the wire, and only half of it is re-derived.**
/// `AddonQtyRange` stamps `format!("{}|{}", subject.subject(), rule.addon_sku_id)`
/// and [`PlanShape::subject`] renders `{plan_id}/{revision}`, so the 400 this door
/// answers names `00000000-0000-0000-0000-000000000000/0|{addonSkuId}` —
/// `error_mapping` passes every violation's `subject` through unchanged. The SKU
/// half locates the fault; the plan half names a plan that does not exist while
/// the one under edit is in the URL. "The violation's subject is re-derived from
/// the rules themselves" is true of the suffix and of nothing else, and the two
/// doors that inherit this paragraph by reference —
/// [`require_authorable_period_bounds`] and
/// [`require_authorable_purchase_window`] — have no suffix at all.
fn require_authorable_addon_bounds(rules: &[AddonRule]) -> Result<(), DomainError> {
    let mut subject = PlanShape::new(PlanId::new(Uuid::nil()), 0, OffsetDateTime::now_utc());
    subject.addon_rules = rules.to_vec();
    let report = ValidationPipeline::new()
        .with_rule(Box::new(AddonQtyRange))
        .run(&subject);
    if report.is_publishable() {
        return Ok(());
    }
    Err(DomainError::ValidationFailed(without_placeholder_plan(
        report,
    )))
}

/// The duplicate-key doors the three wholesale-replace facets were missing.
///
/// Each of these keys is a PRIMARY KEY or UNIQUE index on the table its facet
/// replaces, so a payload naming one twice was refused by the **store** and
/// reached the caller as an untyped **500 "An internal error occurred. Please
/// retry later."** — measured on the deployed platform for all three, and for a
/// blank `output_unit` besides.
///
/// # Why these answer `InvalidRequest` and not a code of their own
///
/// A refusal code in this gear is **normative vocabulary**: `plan_rules`'
/// `DECLARED` list pairs every constant with its spelling in the design set's
/// §5, and `plan_rules_tests` refuses a constant the document does not carry —
/// deliberately, so a code cannot be minted at a REST door. Four were minted
/// here first and withdrawn when that gate caught them; adding them properly is
/// a design-set change and an owner's decision, not part of a fix wave.
///
/// [`DomainError::InvalidRequest`] is the existing home for exactly this class,
/// and its own doc says so: "a request the gear cannot interpret at all". A set
/// naming one add-on twice with two different bounds does not say which bounds
/// apply, and a blank output unit names nothing a meter can be read under. It
/// renders `invalid_argument` — **400** — carrying the duplicated value, which is
/// the whole distance from where these three were.
///
/// The first duplicate is named rather than all of them: `InvalidRequest` is one
/// string, not the enumerated envelope the publish pre-check answers. That is a
/// real loss against `require_authorable_addon_bounds` beside it, and the price
/// of not inventing vocabulary. A typed code would restore it.
fn require_distinct_addon_skus(rules: &[AddonRule]) -> Result<(), DomainError> {
    let mut seen: BTreeSet<Uuid> = BTreeSet::new();
    for rule in rules {
        if !seen.insert(rule.addon_sku_id) {
            return Err(DomainError::InvalidRequest(format!(
                "add-on {} is named more than once in this rule set; two rules for one SKU do \
                 not say which bounds apply",
                rule.addon_sku_id
            )));
        }
    }
    Ok(())
}

fn require_authorable_composites(
    composites: &[crate::domain::plan_shape::CompositeMeter],
) -> Result<(), DomainError> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for composite in composites {
        let unit = composite.output_unit.trim();
        if unit.is_empty() {
            return Err(DomainError::InvalidRequest(
                "a composite must name the unit it outputs; a blank outputUnit names nothing \
                 a meter can be read under"
                    .to_owned(),
            ));
        }
        if !seen.insert(unit) {
            return Err(DomainError::InvalidRequest(format!(
                "two composites of this revision output `{unit}`; the cycle check builds one \
                 edge set per output unit, so a second definition merges into the first rather \
                 than standing beside it"
            )));
        }
    }
    Ok(())
}

fn require_distinct_period_markets(
    bounds: &[crate::domain::plan_shape::PeriodFloorCap],
) -> Result<(), DomainError> {
    let mut seen: BTreeSet<(&str, &str)> = BTreeSet::new();
    for bound in bounds {
        if !seen.insert((bound.currency.as_str(), bound.region.as_str())) {
            return Err(DomainError::InvalidRequest(format!(
                "market {}/{} carries more than one period floor/cap in this set",
                bound.currency, bound.region
            )));
        }
    }
    Ok(())
}

/// Refuse a period floor/cap the store will refuse anyway (**D-319**).
///
/// [`require_authorable_addon_bounds`]' arrangement, one table over and for the
/// identical reason. `pricing_plan_period_floor_cap` carries four `CHECK`s —
/// `floor_minor > 0`, `cap_minor > 0`, `floor_minor <= cap_minor`, and at least one
/// of the two present — and `MinorAmount::new(0)` is `Ok` by design, so a `0`
/// bound, an inverted pair and a bound-less entry all parsed, all reached the
/// insert, and all came back `500 … please retry later` carrying a constraint name.
///
/// **All four arms of `inst-pfc-amount` were dead at publish**, which is the half
/// that made this more than a bad status: the `CHECK`s guarantee no *stored* row can
/// ever be the subject the rule judges, and every `run_publish_rules` subject is
/// assembled from storage, so `PERIOD_FLOOR_CAP_AMOUNT_INVALID` was a declared code
/// with no raiser. Running the rule here gives it one.
///
/// **The whole rule, not one arm of it** — the difference from `inst-ph-trial` at the
/// phases door. Each of the four faults is read from the submitted bound alone, the
/// facet is a wholesale replace so the resolving call retracts what was just sent,
/// and each is refused by a `CHECK`: there is no arm of this rule that describes a
/// legitimate intermediate draft. So the door asks the pipeline's own question,
/// `is_publishable()`, exactly as the add-on door does, rather than stamping four
/// write-stage arms on a rule the publish pipeline also registers.
///
/// `PeriodFloorCapMarketSold` stays out and is not an omission: its operand is the
/// plan's whole row set, which is not in this request.
///
/// The plan id and revision are placeholders for the add-on door's reason: this rule
/// reads `period_floor_caps` and nothing else.
///
/// **Unlike that door, nothing here re-derives the subject.**
/// `PeriodFloorCapAmounts` stamps `subject.subject()` — the placeholder pair, so
/// `00000000-0000-0000-0000-000000000000/0` — and names the offending market only
/// inside the free-text `detail`. `error_mapping` puts that on the wire unchanged,
/// so a client branching on `subject`, which is what the design set says the field
/// is for, is told the fault belongs to a plan that does not exist. Strictly better
/// than the 500 it replaced, and recorded rather than left implicit because the
/// repair is a choice: the real `plan_id`/`revision`, which the `PATCH` holds and
/// the create does not, or the market in the violation's subject the way
/// `AddonQtyRange` names its SKU.
fn require_authorable_period_bounds(bounds: &[PeriodFloorCap]) -> Result<(), DomainError> {
    let mut subject = PlanShape::new(PlanId::new(Uuid::nil()), 0, OffsetDateTime::now_utc());
    subject.period_floor_caps = bounds.to_vec();
    let report = ValidationPipeline::new()
        .with_rule(Box::new(PeriodFloorCapAmounts))
        .run(&subject);
    if report.is_publishable() {
        return Ok(());
    }
    Err(DomainError::ValidationFailed(without_placeholder_plan(
        report,
    )))
}

/// Refuse a purchase-quantity window that admits no quantity.
///
/// `chk_pricing_plan_purchase_qty` refuses `purchase_min_qty > purchase_max_qty` at
/// the column, and `PURCHASE_QTY_RANGE_INVALID` — the code that names the fault —
/// was **unraisable**: `inst-cs-onetime` reports at publish over a shape
/// `publish::assemble_from` loaded back from storage, and the `CHECK` guarantees
/// that shape can never carry the fault. So the only path the pair had was the
/// `INSERT`, and both verbs answered `500 … please retry later` for a payload whose
/// whole remedy is one field. `PlanRepo`'s module doc argued the rule was kept for
/// "a caller validating a candidate shape before writing it"; this is that caller.
///
/// **Both verbs, because it hangs off the parse.** [`shape_of`] and [`shape_patch`]
/// each call it, which is [`require_well_formed_plan_name`]'s arrangement and puts
/// the refusal ahead of every write — including `target_revision`, which on a plan
/// with no open draft *opens a successor*.
///
/// **Its subject is the placeholder pair too.** `PurchaseQtyRange` stamps
/// `subject.subject()` and names the two quantities only in `detail`, so the 400
/// carries `00000000-0000-0000-0000-000000000000/0` —
/// [`require_authorable_period_bounds`] pays the same cost and records what the
/// repair would be.
///
/// **What it cannot see on its own is a `PATCH` that sends one bound against a
/// stored other.** `PlanShapePatch` moves whichever members the caller named, so
/// `{"purchase_min_qty": 5}` on a draft already holding a `purchase_max_qty` of 2
/// carries no fault this function can read. That half is closed by
/// [`require_authorable_windows_against_the_stored_row`], which merges the payload
/// onto the row the caller's `If-Match` names and calls this function with the
/// pair; this one stays because it needs no read at all, so it also holds on the
/// create and on a `PATCH` whose tag names no row that door can resolve.
fn require_authorable_purchase_window(
    min: Option<u64>,
    max: Option<u64>,
) -> Result<(), DomainError> {
    let mut subject = PlanShape::new(PlanId::new(Uuid::nil()), 0, OffsetDateTime::now_utc());
    subject.purchase_min_qty = min;
    subject.purchase_max_qty = max;
    let report = ValidationPipeline::new()
        .with_rule(Box::new(PurchaseQtyRange))
        .run(&subject);
    if report.is_publishable() {
        return Ok(());
    }
    Err(DomainError::ValidationFailed(without_placeholder_plan(
        report,
    )))
}

/// Refuse an availability window that closes before it opens.
///
/// `chk_pricing_plan_availability` is `available_to > available_from`, and **no rule
/// anywhere judges the ordering**: `AvailabilityInsideCoverage` reads `available_to`
/// against a key's window coverage, never against `available_from`. So the pair had
/// the same single path the purchase window had, and the same 500.
///
/// It answers [`DomainError::InvalidRequest`] rather than an enumerated report, and
/// that is the surface's own precedent rather than a shortcut: [`frequency_of`]
/// refuses a token/interval pairing the store would refuse the same way, for the
/// reason stated there — "rather than being discovered by a `CHECK` and reported as
/// a storage failure". No code in the design set names this fault, and minting one
/// here would put a code on the wire that no document declares.
///
/// Equality is refused with inversion, because the `CHECK` is strict: a window that
/// opens and closes at the same instant is open for no time at all.
///
/// It reads only the submitted pair, exactly as
/// [`require_authorable_purchase_window`] does, and the one-bound-at-a-time payload
/// is closed the same way — through
/// [`require_authorable_windows_against_the_stored_row`], which merges before it
/// calls this.
fn require_ordered_availability_window(
    available_from: Option<OffsetDateTime>,
    available_to: Option<OffsetDateTime>,
) -> Result<(), DomainError> {
    let (Some(from), Some(to)) = (available_from, available_to) else {
        return Ok(());
    };
    if to > from {
        return Ok(());
    }
    Err(DomainError::InvalidRequest(format!(
        "availableTo {} is not after availableFrom {}: the window is open for no instant at \
         all, so the plan is authorable and unsellable",
        format_rfc3339(to),
        format_rfc3339(from)
    )))
}

/// Judge the shape facet's two window rules against the **merged** pair: what this
/// `PATCH` sends, over what the revision it lands on already holds.
///
/// [`require_authorable_purchase_window`] and
/// [`require_ordered_availability_window`] read the submitted payload, which closes
/// the fault only when a caller sends both bounds at once. Each rule's operand is a
/// **pair**, so `{"purchase_min_qty": 5}` on a draft holding a `purchase_max_qty` of
/// 2 is the identical fault arriving one field at a time — and it reached
/// `chk_pricing_plan_purchase_qty` and answered `500 … please retry later` for a
/// payload whose whole remedy is one field, which is what those two doors exist to
/// stop.
///
/// # One read, and the `If-Match` is what makes it one
///
/// The two obvious placements are both wrong, which is why this was left owed when
/// those doors landed. Behind [`target_revision`] the operand is free — that call
/// has already resolved the row — but on a plan with no open draft it *writes*, so a
/// refusal there leaves a successor revision standing on a call the author was told
/// did not happen: the defect D-342's phase door was reordered to remove, pinned by
/// `a_refused_phases_write_on_a_published_plan_opens_no_successor`. Ahead of it,
/// resolving the row the way `target_revision` does would be that function's
/// draft-then-current ladder written twice. (Inside the write's own transaction is
/// no cheaper: `plan_repo::update_draft_on` is a blind compare-and-swap that reads
/// no row before it writes, and it commits after the successor already exists.)
///
/// Neither is needed, because the caller has already named the row: `PATCH
/// /plans/{planId}` requires an `If-Match`, and [`require_same_revision`] refuses
/// the call unless the tag's revision **is** the one the patch acts on — the open
/// draft on one arm, and on the other the **current** revision, whose bounds
/// `PlanRepo::open_revision` copies into the successor verbatim. So one
/// `find_revision` on the asserted number reads exactly the row the merge is about,
/// on both arms, before anything is opened. That is [`require_no_stranded_rows`]'s
/// arrangement — a door needing a read, run in [`patch_plan`] ahead of the
/// resolution — with the difference that here the revision is the operand's address
/// rather than a label on a finding.
///
/// # What it does not judge, and what it costs
///
/// **A tag naming no row is not this door's refusal.** `find_revision` answering
/// `None` returns `Ok`: the caller is stale, the plan is spent, or it never existed,
/// and [`target_revision`] discriminates those with the code each deserves.
/// Refusing here would answer a payload fault to a caller whose real problem is the
/// precondition.
///
/// **Neither rule can refuse a legitimate intermediate draft**, which is what makes
/// merging safe rather than stricter than the store: both faults are refused by a
/// column `CHECK`, so the merged row this door judges is a row no `UPDATE` could
/// have stored anyway. A draft holding `max = 2` that wants `min = 5` has to send
/// both members in one payload whatever this surface does.
///
/// The cost is one extra read on every shape `PATCH` and, [`require_no_stranded_rows`]'s
/// own stated cost, that a caller wrong about both the payload and the tag is told
/// about the payload first. Neither refusal is lost.
///
/// # Errors
/// [`DomainError::ValidationFailed`] carrying `PURCHASE_QTY_RANGE_INVALID` when the
/// merged quantity window admits no quantity; [`DomainError::InvalidRequest`] when
/// the merged availability window closes at or before it opens; [`RepoError`]'s
/// ladder on a storage failure.
async fn require_authorable_windows_against_the_stored_row(
    state: &AuthoringState,
    scope: &AccessScope,
    tenant: Uuid,
    plan_id: PlanId,
    revision: u64,
    patch: &PlanShapePatch,
) -> Result<(), CanonicalError> {
    let Some(stored) = state
        .plans
        .find_revision(scope, tenant, plan_id, revision)
        .await
        .map_err(|e| CanonicalError::from(repo_failure(&e)))?
    else {
        return Ok(());
    };
    // `or`, not `unwrap_or`: an omitted member of `PlanShapePatch` means "leave it
    // alone" and never "clear it" (that type's own doc), so a submitted value wins
    // where there is one and the stored value stands where there is not. The pair
    // that comes out is the row `patched_columns` is about to write.
    require_authorable_purchase_window(
        patch.purchase_min_qty.or(stored.purchase_min_qty),
        patch.purchase_max_qty.or(stored.purchase_max_qty),
    )
    .map_err(CanonicalError::from)?;
    require_ordered_availability_window(
        patch.available_from.or(stored.available_from),
        patch.available_to.or(stored.available_to),
    )
    .map_err(CanonicalError::from)
}

/// Build the Axum router for the plan surface and register its operations.
///
/// The declared responses are the ones each path can actually produce. **No 422
/// anywhere**: §3.3's status-rendering rule makes every architectural 422 in the
/// design set reach the wire as a 400 carrying its code, so a 422 in an
/// `OpenAPI` registration here would document a response no path can emit.
#[allow(
    clippy::too_many_lines,
    reason = "one builder chain per operation; flat is clearer than helpers that hide which route declares which response"
)]
pub fn router(state: Arc<AuthoringState>, openapi: &dyn OpenApiRegistry) -> Router {
    let mut router = Router::new();

    router = OperationBuilder::get("/bss-pricing/v1/plans")
        .operation_id("bss_pricing.list_plans")
        .summary("List the tenant's plans (cursor-paginated)")
        .description(
            "One row per plan: choose its open draft, else current published/retired revision, \
             then apply filters, ordering and keyset pagination. Historical revisions cannot \
             match filters on the displayed name/state. Default order is plan_id asc; supported \
             sort fields are plan_id, plan_name, lifecycle_state, billing_cycle, created_at and \
             price_row_count. plan_id asc is appended as a unique tie-breaker when absent; \
             nullable names/cycles sort last in both directions. limit defaults to 100, max 1,000. \
             Repeat the same $filter with cursor; omit $orderby on subsequent pages. \
             plan_name supports exact equality and case-insensitive contains/startswith/endswith. \
             Query-only model_kind and currency predicates independently match draft/published \
             price rows (possibly different rows); response arrays stay model_kinds/currencies. \
             Example: $filter=model_kind eq 'flat' and currency in ('EUR','USD'). \
             has_pending_approvals eq true tests the same direct submitted units as pending_approvals. \
             created_at is the original plan creation time; revision_created_at is the shown \
             revision's creation time. No created_at_utc alias. Full shape remains on GET /plans/{planId}. \
             Cache-Control: private, no-store.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .query_param_typed(
            "limit",
            false,
            "Plans per page (default 100, hard cap 1,000)",
            "integer",
        )
        .query_param("cursor", false, "Opaque base64url pagination cursor")
        .handler(list_plans)
        .with_odata_filter::<PlanFilterField>()
        .with_odata_orderby::<PlanOrderField>()
        .json_response_with_schema::<Page<PlanSummaryView>>(
            openapi,
            StatusCode::OK,
            "One page of the tenant's plans.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    // D-360. Registered ahead of the `{planId}` routes so the static segment is
    // unambiguous; the route census pins the pair.
    router = OperationBuilder::get("/bss-pricing/v1/plans/counts")
        .operation_id("bss_pricing.count_plans")
        .summary("Count the tenant's plans by authoring state")
        .description(
            "How many plans the tenant holds as `draft`, `published` and `retired`, counted the \
             way the list renders them: a plan with an open draft counts as `draft`, else as its \
             current revision's state (D-360). Over the whole catalogue - a tab strip built from \
             one page of `GET /plans` is wrong past that page. `total` is the number of plans and \
             the three counts sum to it. Gates on `plan` x `read`, like the list.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .handler(count_plans)
        .json_response_with_schema::<PlanCountsView>(openapi, StatusCode::OK, "The counts.")
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/bss-pricing/v1/plans")
        .operation_id("bss_pricing.create_plan")
        .summary("Create a draft plan")
        .description(
            "Creates a plan by writing its revision `0` in `draft`, and answers `201` with a \
             `Location` header naming the new plan and an `ETag` carrying the revision's row \
             version. The plan id is **minted by the server**, so a caller never has to choose \
             one and a retry can never create a second plan under a name the client picked. \
             An `Idempotency-Key` header is **required** (S2 API surface, foundation step 1): the gate is the \
             first step of the mutation and runs in the same transaction as it, so a retry \
             carrying the same key and the same body is answered the recorded response - the \
             original plan id included: and a retry carrying the same key and a different body \
             is refused `IDEMPOTENCY_PAYLOAD_MISMATCH`. Nothing here judges the plan's shape: \
             the Slice-2 rules run at publish.\n\nThe answer carries **one phase**: the implicit \
             terminal `evergreen` phase, auto-created with the plan (`inst-ph-default`, D-19) \
             because a price row's `scopeKey.phase` is a required `Uuid` with no server-side \
             default, and the phase axis defaults to the plan's terminal phase. Read its \
             `phase_id` out of this response rather than minting one: a phase id the store did \
             not choose can collide, and a row keyed on a phase the revision does not attach is \
             refused at publish (`PHASE_ROW_ORPHANED`) with no way to re-point the row.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .param(idempotency_key_param())
        .json_request::<PlanShapeRequest>(openapi, "The plan's initial shape.")
        .handler(create_plan)
        .json_response_with_schema::<PlanView>(
            openapi,
            StatusCode::CREATED,
            "The plan's newly created revision `0`.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::patch("/bss-pricing/v1/plans/{planId}")
        .operation_id("bss_pricing.patch_plan")
        .summary("Edit a plan's open draft revision")
        .description(
            "Applies **exactly one** facet - the plan's own columns, its phase chain, its \
             add-on rule set, its descriptor set, its derived (composite) meter set, or its \
             period floor/cap set - to the plan's open draft revision, under \
             the `If-Match` precondition. Two facets in one body is `400`: each child mutator \
             compare-and-swaps on the revision's row version and bumps it, so the second could \
             not match the tag the first advanced. When the plan holds **no** open draft, a \
             successor revision is opened from its current one and the patch lands on that \
             (D-90); the `If-Match` is then tested against the current revision. A retired plan \
             answers `PLAN_RETIRED_NO_SUCCESSOR`, a plan that already holds a draft answers \
             `OPEN_DRAFT_REVISION_EXISTS`, and a plan whose every revision is `abandoned` \
             answers `PLAN_ABANDONED_NO_SUCCESSOR` - its id is spent, so mint a new plan.\n\nThe \
             **phases** facet is a wholesale replace and is judged when it is made (D-342): a \
             payload that drops a phase one of the revision's price rows keys on - the empty set \
             included - is refused `PHASE_ROW_ORPHANED`, naming the phase, rather than being \
             accepted and refused at publish, by which time a published row cannot be re-pointed \
             and a draft row can only be deleted. A replace that keeps every phase the rows name \
             still succeeds. The submitted chain is judged against itself at the same door: a set \
             with no terminal phase or with two, a `convertsToPhaseId` naming no phase of the set, \
             and a cycle are each refused `PHASE_GRAPH_INVALID` - the facet replaces the phase set \
             wholesale, so none of those is a state a later call completes. A payload naming one \
             `phase_id` twice answers `PHASE_ID_IN_USE` (`409`, D-340).",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("planId", "The plan to edit.")
        .param(if_match_param(
            "On a plan route the tag names **both** the revision it was read from and that \
             revision's version, joined by a hyphen (for example `\"3-7\"`), because \
             `/plans/{planId}` does not name a revision and a version alone would be applied \
             to whichever revision this call resolves.",
        ))
        .json_request::<PatchPlanRequest>(openapi, "Exactly one facet of the plan's shape.")
        .handler(patch_plan)
        .json_response_with_schema::<PlanView>(
            openapi,
            StatusCode::OK,
            "The revision as it stands after the edit.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/bss-pricing/v1/plans/{planId}/abandon")
        .operation_id("bss_pricing.abandon_plan_draft")
        .summary("Discard a plan's open draft revision")
        .description(
            "Flips the plan's open draft revision to the terminal `abandoned` state under the \
             `If-Match` precondition, dropping its phase, add-on-rule and descriptor-set copies \
             in the same transaction (D-145). It is **not** a delete and the verb is therefore \
             not `DELETE`: the row survives as a tombstone so the `revision` number it consumed \
             stays consumed and `(planId, revision)` never names two rows over a plan's \
             lifetime. The precondition is what an unconditional abandon would destroy - a \
             concurrent editor's uncommitted work. A plan holding no open draft answers \
             `LIFECYCLE_FORBIDDEN`; a plan whose every revision is already `abandoned` answers \
             `PLAN_ABANDONED_NO_SUCCESSOR`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("planId", "The plan whose open draft is discarded.")
        .param(if_match_param(
            "On a plan route the tag names **both** the revision it was read from and that \
             revision's version, joined by a hyphen (for example `\"3-7\"`); here it must name \
             the plan's open draft.",
        ))
        .handler(abandon_plan_draft)
        .json_response_with_schema::<PlanView>(
            openapi,
            StatusCode::OK,
            "The tombstoned revision, at its last row version.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::post("/bss-pricing/v1/plans/{planId}/clone")
        .operation_id("bss_pricing.clone_plan")
        .summary("Clone a plan into a new draft plan")
        .description(
            "Copies the source plan's **current** revision into a brand-new plan at revision \
             `0` in `draft`, with new ids throughout: a new `planId`, new `phaseId`s with every \
             reference to them remapped, new `priceId`s, a new `compositeId` per meter and, \
             where the source is a bundle, a new `bundleId` over a copied composition (D-269). \
             `clonedFrom` records the source. The **whole clone is one transaction** - a \
             failure part way leaves no plan behind (D-275).\n\nWhat is copied is *configuration*; what is left behind is *lifecycle state*, and \
             the response says so rather than leaving it to be discovered at the clone's first \
             publish: `PriceWindow` schedules are never cloned, so the clone's billable rows \
             have no coverage and its publish stays blocked until fresh windows are scheduled \
             (`inst-cl-windows`); and both cutover-made eligibility classes - \
             `existing_grandfathered` and `new_subscriptions_only` - stay behind, counted per \
             class (`inst-cl-resets`, D-268).\n\nThe clone is an ordinary draft: no rule reads `clonedFrom`, and its first publish \
             takes the full pipeline and an approval like any other first publish \
             (`inst-cl-draft`). Guarded at-most-once on the `Idempotency-Key`; a replay answers \
             the first caller's plan id rather than cloning twice.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param(
            "planId",
            "The plan to clone. Its **current** revision is the source - which a retired plan \
             still has, so a retired plan is clonable and deliberately so: retirement closes \
             the plan to further revisions, and the clone route forward is what an operator \
             has instead (D-145 as amended). A plan holding only a draft has no current \
             revision and answers `CLONE_SOURCE_NOT_FOUND`.",
        )
        .param(idempotency_key_param())
        .handler(clone_plan)
        .json_response_with_schema::<CloneReceiptView>(
            openapi,
            StatusCode::CREATED,
            "The new plan, what came across, and what deliberately did not.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/bss-pricing/v1/plans/{planId}")
        .operation_id("bss_pricing.get_plan")
        .summary("Read a plan's authoring revision")
        .description(
            "Returns the plan's **open draft** revision when it has one, and its **current** \
             revision (`published` or `retired`) otherwise, with all five of its child sets - \
             its phase chain, its add-on rules, its descriptor set, its derived (composite) \
             meters and its period floor/cap. This is the authoring read (`plan` x `read`); the \
             published content a consumer resolves comes from the read model, which is a \
             different contract. `lifecycle_state` and `revision` name which revision was \
             answered, so a caller never infers it. The `ETag` header carries the revision's row \
             version and is what the mutating verbs take as `If-Match`. It is not a cache \
             validator for derived pending metadata: this GET returns a fresh body with \
             `Cache-Control: private, no-store` and does not evaluate `If-None-Match`. \
             A plan outside the \
             caller's scope reads exactly like an absent one (404, no existence leak).",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("planId", "The plan to read.")
        .handler(get_plan)
        .json_response_with_schema::<PlanView>(
            openapi,
            StatusCode::OK,
            "The plan's open draft revision, or its current revision.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
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

/// `GET /plans/{planId}`.
///
/// **Which revision it answers, and why that is a decision rather than a
/// lookup.** A plan is a chain, and at most two of its revisions are reachable
/// for authoring: the open `draft`, and the current one
/// ([`LifecycleState::is_current_revision`](crate::domain::lifecycle::LifecycleState::is_current_revision),
/// never a hand-written state list). The draft wins when there is one, because
/// an author editing a plan is working on the draft and a read that answered the
/// published revision would hand them a body their next `PATCH` would not match.
/// With no draft the current revision is the only thing a caller can act on  -
/// a `PATCH` opens a successor from it.
///
/// The gate is `plan x read` with `owner_tenant_id = None`, so the PDP derives
/// the scope from the subject rather than from anything the caller sent, and the
/// compiled scope is the SQL filter. `require_constraints = true` so an
/// unconstrained allow fail-closes instead of exposing every tenant's plans.
async fn get_plan(
    Extension(state): Extension<Arc<AuthoringState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Path(plan_id): Path<Uuid>,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let plan_id = PlanId::new(plan_id);
    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::PLAN,
        crate::authz::actions::READ,
        /* owner_tenant_id */ None,
        /* resource_id */ Some(crate::authz::ResourceRef(plan_id.get())),
    )
    .await
    .map_err(authz_error_to_canonical)?;

    let tenant = ctx.subject_tenant_id();
    let revision = authoring_revision(&state.plans, &scope, tenant, plan_id)
        .await
        .map_err(|e| CanonicalError::from(repo_failure(&e)))?
        .ok_or_else(|| CanonicalError::from(not_readable(plan_id)))?;

    let number = revision.revision;
    let view = read_shape(&state, &scope, tenant, plan_id, number, revision)
        .await
        .map_err(|e| CanonicalError::from(repo_failure(&e)))?;
    let tag = plan_tag(&view);
    // Always enrich: the authored-content tag cannot validate pending metadata.
    //
    // Asked of the service rather than of the repository, because the scope this
    // read needs is **not** the door's: the door pins `resource_id = plan_id` and
    // `pricing_approval` is keyed on `approval_id`.
    // `ApprovalService::pending_for_plans` carries that whole argument and does
    // the narrowing where a narrowed scope is sanctioned — a handler that minted
    // one would bypass the PEP structurally, which `rest_authz` bans outright. No
    // second PDP question is asked here: `plan × read` on this plan is the whole
    // gate, and the field says only that a unit exists and which one.
    let mut view = view;
    view.pending_approvals = state
        .approvals
        .pending_for_plans(tenant, &[plan_id])
        .await
        .map_err(|e| CanonicalError::from(repo_failure(&e)))?
        .remove(&plan_id)
        .unwrap_or_default()
        .into_iter()
        .map(|unit| PendingApprovalView {
            approval_id: unit.approval_id,
            subject_kind: unit.subject_kind.as_str().to_owned(),
            submitted_at: unit.submitted_at,
        })
        .collect();

    Ok(preconditions::fresh_read(([(ETAG, tag)], Json(view))))
}

/// `GET /plans`.
///
/// The collection read, gated exactly as [`get_plan`] is except that
/// `resource_id` is `None`: there is no single resource to name, so what the PDP
/// compiles is the tenant filter the walk runs under. `require_constraints` is
/// `true` for the same reason as everywhere else on this plane — an
/// unconstrained allow must fail closed rather than page through every tenant's
/// catalogue.
///
/// Each plan is rendered as its **authoring** revision, which is
/// [`authoring_revision`]'s rule applied to a page rather than to one id;
/// `plan_repo::list_authoring_page` holds the argument for why a listing over
/// current revisions would be the wrong one.
#[allow(clippy::implicit_hasher)]
async fn list_plans(
    Extension(state): Extension<Arc<AuthoringState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Query(extras): Query<HashMap<String, String>>,
    OData(odata): OData,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    reject_non_odata_list_params(&extras)?;
    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::PLAN,
        crate::authz::actions::READ,
        /* owner_tenant_id */ None,
        /* resource_id */ None,
    )
    .await
    .map_err(authz_error_to_canonical)?;

    let tenant = ctx.subject_tenant_id();
    let page = state
        .plans
        .list_authoring_odata(&scope, tenant, &odata)
        .await
        .map_err(map_odata_page_err)?;

    // D-359: one read of the tenant's pending units for the whole page, marked
    // onto the rows here. Not a lookup per row, and not a column: the set is
    // bounded by how many units are open. `get_plan`'s comment carries the reason
    // this goes through the service.
    let ids: Vec<PlanId> = page
        .items
        .iter()
        .map(|entry| entry.revision.plan_id)
        .collect();
    let mut pending = state
        .approvals
        .pending_for_plans(tenant, &ids)
        .await
        .map_err(|e| CanonicalError::from(repo_failure(&e)))?;

    // The page IDs were authorized under the original plan scope. Project that
    // exact set to child metadata; a plan ID must never be used as a price ID.
    let conn = state.db.conn().map_err(|e| {
        CanonicalError::from(repo_failure(&RepoError::Db(format!(
            "plan aggregates conn: {e}"
        ))))
    })?;
    let aggregates = crate::infra::storage::repo::price_repo::aggregate_rows_for_authorized_plans(
        &conn, tenant, &ids,
    )
    .await
    .map_err(|e| CanonicalError::from(repo_failure(&e)))?;

    Ok(preconditions::fresh_read(Json(Page {
        items: page
            .items
            .iter()
            .map(|entry| {
                let revision = &entry.revision;
                let mut item = PlanSummaryView::from(entry);
                item.pending_approvals = pending
                    .remove(&revision.plan_id)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|unit| PendingApprovalView {
                        approval_id: unit.approval_id,
                        subject_kind: unit.subject_kind.as_str().to_owned(),
                        submitted_at: unit.submitted_at,
                    })
                    .collect();
                if let Some(aggregate) = aggregates.get(&revision.plan_id) {
                    item.price_row_count = aggregate.price_row_count;
                    item.model_kinds.clone_from(&aggregate.model_kinds);
                    item.currencies.clone_from(&aggregate.currencies);
                }
                item
            })
            .collect(),
        page_info: page.page_info,
    })))
}

/// `GET /plans/counts` (D-360): the list's gate - `plan` x `read`, no resource -
/// asked of the PDP once, then one read over the authoring set.
async fn count_plans(
    Extension(state): Extension<Arc<AuthoringState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
) -> Result<Json<PlanCountsView>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let scope = crate::authz::access_scope(
        &enforcer,
        &ctx,
        &crate::authz::resource_types::PLAN,
        crate::authz::actions::READ,
        /* owner_tenant_id */ None,
        /* resource_id */ None,
    )
    .await
    .map_err(authz_error_to_canonical)?;
    let conn = state.db.conn().map_err(|e| {
        CanonicalError::from(repo_failure(&RepoError::Db(format!(
            "plan counts conn: {e}"
        ))))
    })?;
    let counts = plan_repo::count_plans_by_authoring_state(&conn, &scope, ctx.subject_tenant_id())
        .await
        .map_err(|e| CanonicalError::from(repo_failure(&e)))?;
    Ok(Json(PlanCountsView {
        total: counts.total,
        draft: counts.draft,
        published: counts.published,
        retired: counts.retired,
    }))
}

/// The revision an authoring caller is working with: the open draft, else the
/// current revision.
async fn authoring_revision(
    plans: &PlanRepo,
    scope: &toolkit_db::secure::AccessScope,
    tenant: Uuid,
    plan_id: PlanId,
) -> Result<Option<PlanRevision>, RepoError> {
    if let Some(draft) = plans.find_open_draft(scope, tenant, plan_id).await? {
        return Ok(Some(draft));
    }
    plans.find_current(scope, tenant, plan_id).await
}

/// Attach a revision's five child sets.
async fn read_shape(
    state: &AuthoringState,
    scope: &toolkit_db::secure::AccessScope,
    tenant: Uuid,
    plan_id: PlanId,
    revision: u64,
    row: PlanRevision,
) -> Result<PlanView, RepoError> {
    let phases = state
        .shapes
        .list_phases(scope, tenant, plan_id, revision)
        .await?;
    let addon_rules = state
        .shapes
        .list_addon_rules(scope, tenant, plan_id, revision)
        .await?;
    let descriptor_set = state
        .shapes
        .find_descriptor_set(scope, tenant, plan_id, revision)
        .await?;
    // The fourth read, and the one the `composites` facet's `composite_id`
    // round trip depends on: a `PATCH` replaces the set wholesale, so an author
    // who cannot read the ids back cannot preserve a definition's identity
    // across an edit (D-106).
    let composites = state
        .shapes
        .list_composites(scope, tenant, plan_id, revision)
        .await?;
    // The fifth read (D-319), for the fourth's reason: the `period_floor_caps`
    // facet replaces the set wholesale, so an author who cannot read the
    // current markets back cannot add one without dropping the rest.
    let period_floor_caps = state
        .shapes
        .list_period_floor_caps(scope, tenant, plan_id, revision)
        .await?;
    Ok(PlanView::new(
        row,
        state.plans.created_at(scope, tenant, plan_id).await?,
        phases,
        addon_rules,
        descriptor_set,
        composites,
        period_floor_caps,
    ))
}

/// The "absent, or not yours, or holding nothing an author can act on" refusal.
///
/// One answer for all three, which is the posture `frontier.rs` already
/// documents: a 403 for a foreign tenant's plan would confirm that the plan
/// exists, and the catalog is commercially sensitive.
fn not_readable(plan_id: PlanId) -> DomainError {
    DomainError::NotFound {
        subject: "plan".to_owned(),
        id: plan_id.to_string(),
    }
}

// ---------------------------------------------------------------------------
// The writes.
// ---------------------------------------------------------------------------

/// `POST /plans`.
///
/// The gate is `plan x write` with `owner_tenant_id = Some(caller's tenant)`, so
/// `access_scope`'s membership assertion refuses a target outside the compiled
/// scope - the degraded flat-`In` decision does not re-check the property, and a
/// write is the one direction where "the PDP will have filtered it" is not true.
///
/// **The surface mints the plan id** (`plan_repo`'s `NewPlanDraft` says so):
/// a repository that generated ids would make an idempotent retry create a
/// second plan, and the id has to be in a `Location` header before the row is
/// durable. Minting happens **inside** the guarded body, so a replay answers the
/// recorded body carrying the **original** id rather than a fresh one - which is
/// the entire reason the response body is stored.
///
/// A replay reconstructs `Location` from the recorded body's `plan_id` and sets
/// **no** `ETag`: the location is a pure function of an id the record carries,
/// while the revision's row version may have moved since the record was written,
/// and a stale tag is a precondition token that looks valid and is not.
async fn create_plan(
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
        &crate::authz::resource_types::PLAN,
        crate::authz::actions::WRITE,
        /* owner_tenant_id */ Some(crate::authz::OwnerTenant(tenant)),
        /* resource_id */ None,
    )
    .await
    .map_err(authz_error_to_canonical)?;

    // Parsed after the gate, not before it: `prices.rs` orders it this way and a
    // module doc asserting "the gate before the repository" reads as two
    // disciplines if the two modules differ on where the body is read.
    let body: PlanShapeRequest = preconditions::parse_body(&body)?;
    let client_key = preconditions::idempotency_key(&headers)?;
    let request_hash = preconditions::request_digest(&body)?;
    let draft_shape = shape_of(&body)?;
    let now = OffsetDateTime::now_utc();

    let guard = GuardedRequest {
        operation: CREATE_PLAN_OPERATION,
        client_key,
        request_hash,
        tenant_id: tenant,
        status: StatusCode::CREATED.as_u16().into(),
        now,
    };
    let scope_for_body = scope.clone();
    let actor = ctx.subject_id();
    let outcome = idempotent::guarded(
        &state.db,
        &state.idempotency,
        &scope,
        guard,
        move |txn: &DbTx<'_>| -> TxFuture<'_, (PlanRevision, PlanPhase)> {
            Box::pin(async move {
                // Minted here rather than before the claim: a replay must answer
                // the first caller's id, and an id minted outside the guarded
                // body would be a second one nobody was ever told about.
                let (draft, granted) = draft_shape.into_draft(
                    PlanId::new(Uuid::now_v7()),
                    tenant,
                    actor,
                    now,
                    correlation,
                );
                // Mapped here rather than by the gate: `guarded` now takes a mutation
                // that speaks the gear's rejection vocabulary, because a pipeline
                // richer than one repository call has refusals no `RepoError` can
                // carry. The ladder is the same one, at the call site that knows what
                // it produced. `create_granted_draft_on`, not `create_draft_on`: the
                // grant set and the change contract this request authored ride the
                // create's own INSERT, so the plan is not born at row version 1
                // carrying an edit nobody made.
                let revision =
                    plan_repo::create_granted_draft_on(txn, &scope_for_body, draft, granted)
                        .await
                        .map_err(|e| repo_failure(&e))?;

                // D-19's creation-time act (`inst-ph-default`): the `phase`
                // scope-key axis is always a `phase_id` and its default value is
                // the plan's terminal phase, so a plan without one cannot be
                // priced at all — the add-row surface has nothing to key on.
                // Minted here, in the same transaction, for the same reason the
                // plan id is.
                let phase = PlanPhase {
                    phase_id: PhaseId::new(Uuid::now_v7()),
                    kind: PhaseKind::Evergreen,
                    display_name: None,
                    ordinal: 0,
                    converts_to_phase_id: None,
                    phase_duration_days: None,
                    display_trial_days: None,
                };
                plan_shape_repo::seed_terminal_phase_on(
                    txn,
                    &scope_for_body,
                    tenant,
                    &revision,
                    &phase,
                )
                .await
                .map_err(|e| repo_failure(&e))?;

                Ok((revision, phase))
            })
        },
        |created: &(PlanRevision, PlanPhase)| {
            let (revision, phase) = created;
            let view = PlanView::new(
                revision.clone(),
                revision.created_at_utc,
                vec![phase.clone()],
                Vec::new(),
                None,
                Vec::new(),
                Vec::new(),
            );
            serde_json::to_value(&view)
                .map_err(|e| DomainError::Internal(format!("cannot render the created plan: {e}")))
        },
    )
    .await
    .map_err(CanonicalError::from)?;

    Ok(match outcome {
        Guarded::Performed((revision, phase)) => created(&revision, &phase),
        Guarded::Replayed { status, body } => replayed(CREATE_PLAN_OPERATION, status, &body)?,
    })
}

/// `PATCH /plans/{planId}`.
async fn patch_plan(
    Extension(state): Extension<Arc<AuthoringState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    extension_correlation: Option<Extension<CorrelationId>>,
    Path(plan_id): Path<Uuid>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<([(axum::http::HeaderName, String); 1], Json<PlanView>), CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    // Once, at the top: on the arm that opens a successor this call writes
    // **two** records, and D-178 clause (2) is that they carry one value.
    let correlation = require_correlation(extension_correlation)?;
    let tenant = ctx.subject_tenant_id();
    let plan_id = PlanId::new(plan_id);
    let scope = write_scope(&enforcer, &ctx, plan_id.get(), tenant).await?;

    let body: PatchPlanRequest = preconditions::parse_body(&body)?;
    let asserted = preconditions::if_match_revision(&headers)?;
    let facet = Facet::of(body)?;
    let stamp = audit_stamp(&ctx, OffsetDateTime::now_utc(), correlation);

    // D-342's write-stage door, on **this facet and no other**. Before the match
    // rather than inside its arm because every arm answers `RepoError` and this
    // refusal is a validation report.
    //
    // **Before `target_revision`, and the ordering is load-bearing**: on a plan with
    // no open draft that call *writes*
    // — it opens a successor revision and copies the phase set forward — so a door
    // behind it refused the payload and left the successor standing, an edit the
    // author never made on a call they were told did not happen, holding the plan's
    // one open-draft slot until somebody abandons it. The door can run here because
    // it needs no revision to judge: the rows it reads are plan-scoped
    // (`price_repo::load_for_plan` is not filtered by revision) and the number only
    // labels the finding's subject.
    //
    // The cost, stated rather than discovered: on this facet a stranding payload is
    // now refused ahead of the `If-Match` binding and the has-an-editable-revision
    // discrimination, so a caller who is wrong about both is told about the payload
    // first. Both refusals are the caller's own and neither is lost; what the old
    // order bought was a worse diagnosis plus a revision.
    if let Facet::Phases(phases) = &facet {
        require_no_stranded_rows(&state, &scope, tenant, plan_id, asserted.revision, phases)
            .await?;
    }

    // H12's residue, on the shape facet and no other, and here for the reason the
    // door above is here: it needs a read, and `target_revision` writes. See the
    // function for why the revision the `If-Match` names is the row to read, and
    // why one read is enough.
    if let Facet::Shape(patch) = &facet {
        require_authorable_windows_against_the_stored_row(
            &state,
            &scope,
            tenant,
            plan_id,
            asserted.revision,
            patch,
        )
        .await?;
    }

    // The revision the patch lands on, and the version the store will match.
    // It takes the **same** stamp: the successor it may open is the first of the
    // two records this one call writes.
    let target = target_revision(&state, &scope, tenant, plan_id, asserted, stamp).await?;
    let revision = target.revision;
    let expected = target.expected;

    match facet {
        Facet::Shape(shape) => {
            state
                .plans
                .update_draft(&scope, tenant, plan_id, revision, expected, shape, stamp)
                .await
        }
        Facet::Phases(phases) => {
            state
                .shapes
                .replace_phases(&scope, tenant, plan_id, revision, expected, phases, stamp)
                .await
        }
        Facet::AddonRules(rules) => {
            // The one shape check this plane owes, and it is not a policy choice:
            // `pricing_plan_addon_rule` carries three `CHECK`s, so an unsatisfiable
            // quantity bound is refused by the STORE whatever this surface believes.
            // Without this the refusal reached the caller as a **500 naming a
            // database constraint** — `chk_pricing_plan_addon_rule_required_max_qty`
            // for a required add-on with no ceiling — which is what
            // `ADDON_QTY_RANGE_INVALID`'s own doc says it exists to replace.
            //
            // So the header above ("nothing here judges a plan's shape... an
            // author assembles a plan over several calls") holds for every facet
            // whose intermediate states the store accepts, and does not hold here.
            // `require_authorable_addon_bounds` runs the same rule publish runs,
            // reports **all** the offending bounds rather than whichever one the
            // database hit first, and answers the enumerated envelope the publish
            // pre-check answers.
            //
            // **It runs at the parse, in `Facet::of`, and not in this arm** — with
            // `require_distinct_addon_skus` beside it. Both belong ahead of
            // `target_revision`, which this arm runs after, and `Facet::of` carries
            // that argument in full rather than restating it here.
            state
                .shapes
                .replace_addon_rules(&scope, tenant, plan_id, revision, expected, rules, stamp)
                .await
        }
        Facet::DescriptorSet(set) => {
            state
                .shapes
                .set_descriptor_set(&scope, tenant, plan_id, revision, expected, set, stamp)
                .await
        }
        // Slice 10's derived meters. `CompositeArity` and
        // `CompositeSelfReference` stay **publish** rules for the reason this
        // module's header gives - refusing a one-constituent draft at save would
        // contradict the premise that an author assembles a plan over several
        // calls, and the second constituent legitimately arrives in the next one.
        //
        // **But a blank or repeated `outputUnit` is not that.** Both were
        // answered 500 by the store's own constraints, and neither is a state a
        // later call resolves. `require_authorable_composites` refuses them —
        // **at the parse, in `Facet::of`, and not in this arm**, because this arm
        // runs after `target_revision`. So "nothing is judged here" is
        // true of the arm again and false of the facet, which is the reading this
        // paragraph asserted the opposite of while the call sat below it.
        Facet::Composites(composites) => {
            state
                .shapes
                .replace_composites(
                    &scope, tenant, plan_id, revision, expected, composites, stamp,
                )
                .await
        }
        // D-319's period floor/cap. **The amount rule is judged, and not here**:
        // `require_authorable_period_bounds` runs it at the parse, ahead of the
        // successor `target_revision` may open. It is not a publish rule reporting
        // "a fault the table's own `CHECK`s refuse": the `CHECK`s refuse the row
        // first, so the author gets a 500 rather than a report, and no stored row
        // can ever be the subject such a rule judges.
        //
        // **One market named twice** is a separate fault and was its own 500, on the
        // table's PRIMARY KEY; `require_distinct_period_markets`
        // refuses it at the parse too, beside the amount rule in `Facet::of`. It read
        // "below" while the call is ahead of `target_revision` — so "below"
        // points a reader at an arm body that is a bare repository call.
        //
        // `PeriodFloorCapMarketSold` does stay a publish rule: it needs the plan's
        // whole row set, and an author who authors the bound before the market's
        // rows is doing what this module's opening premise expects.
        Facet::PeriodFloorCaps(bounds) => {
            state
                .shapes
                .replace_period_floor_caps(
                    &scope, tenant, plan_id, revision, expected, bounds, stamp,
                )
                .await
        }
    }
    .map_err(|e| CanonicalError::from(repo_failure(&e)))?;

    answer_revision(&state, &scope, tenant, plan_id, revision).await
}

/// `POST /plans/{planId}/abandon`.
///
/// The revision abandoned is the plan's **open draft**, resolved here - which is
/// why the discrimination S2 §5 asks for lives on this surface and not in the
/// repository: `abandon_draft` names a revision, and the question "does this
/// *plan* hold a draft" is about the plan.
async fn abandon_plan_draft(
    Extension(state): Extension<Arc<AuthoringState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    extension_correlation: Option<Extension<CorrelationId>>,
    Path(plan_id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<([(axum::http::HeaderName, String); 1], Json<PlanView>), CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let correlation = require_correlation(extension_correlation)?;
    let tenant = ctx.subject_tenant_id();
    let plan_id = PlanId::new(plan_id);
    let scope = write_scope(&enforcer, &ctx, plan_id.get(), tenant).await?;
    let asserted = preconditions::if_match_revision(&headers)?;

    let draft = state
        .plans
        .find_open_draft(&scope, tenant, plan_id)
        .await
        .map_err(|e| CanonicalError::from(repo_failure(&e)))?;
    let Some(draft) = draft else {
        // No draft: either the id is spent, or the plan has a current revision
        // and there is simply nothing to discard, or it does not exist at all.
        return Err(CanonicalError::from(
            no_open_draft(&state.plans, &scope, tenant, plan_id, "abandon").await,
        ));
    };

    let revision = draft.revision;
    // The tag has to name the revision it was read from. Without it the caller's
    // version is applied to whatever revision the plan currently offers, and a
    // tag minted against revision N satisfies the swap on N+1 - a freshly opened
    // successor starts at version 0, the value every first draft carries.
    require_same_revision(plan_id, asserted, revision).map_err(CanonicalError::from)?;
    let expected = asserted.version;
    state
        .plans
        .abandon_draft(
            &scope,
            tenant,
            plan_id,
            revision,
            expected,
            audit_stamp(&ctx, OffsetDateTime::now_utc(), correlation),
        )
        .await
        .map_err(|e| CanonicalError::from(repo_failure(&e)))?;

    answer_revision(&state, &scope, tenant, plan_id, revision).await
}

// ---------------------------------------------------------------------------
// The pieces the three writes share.
// ---------------------------------------------------------------------------

/// The idempotency-gate operation name for `POST /plans`.
///
/// A per-route constant, so one client key reused across two verbs does not
/// collide: `operation` is part of the dedup table's composite key.
const CREATE_PLAN_OPERATION: &str = "bss_pricing.create_plan";

/// The idempotency operation the clone claims under.
///
/// Distinct from the create's: the two write the same table and a client key is
/// unique **per operation**, so sharing the name would make a clone and a create
/// under one key collide as a payload mismatch rather than as two acts.
const CLONE_PLAN_OPERATION: &str = "bss_pricing.clone_plan";

/// The `plan x write` gate, spelled once for the three mutating routes.
async fn write_scope(
    enforcer: &authz_resolver_sdk::PolicyEnforcer,
    ctx: &SecurityContext,
    resource_id: Uuid,
    tenant: Uuid,
) -> Result<AccessScope, CanonicalError> {
    crate::authz::access_scope(
        enforcer,
        ctx,
        &crate::authz::resource_types::PLAN,
        crate::authz::actions::WRITE,
        /* owner_tenant_id */ Some(crate::authz::OwnerTenant(tenant)),
        /* resource_id */ Some(crate::authz::ResourceRef(resource_id)),
    )
    .await
    .map_err(authz_error_to_canonical)
}

/// Which of the six facets a `PATCH` carries.
// Built once per request and destructured on the next line, so the 304-byte
// variant is one stack value that never outlives the handler. Boxing it would
// buy an allocation and a deref at every read for no measurable gain.
#[allow(clippy::large_enum_variant)]
enum Facet {
    /// The plan's own columns.
    Shape(PlanShapePatch),
    /// The whole phase chain.
    Phases(Vec<PlanPhase>),
    /// The whole add-on rule set.
    AddonRules(Vec<AddonRule>),
    /// The descriptor set.
    DescriptorSet(DescriptorSet),
    /// The whole derived-meter set (Slice 10 §6).
    Composites(Vec<CompositeMeter>),
    /// The whole period floor/cap set (S2 §6, D-319).
    PeriodFloorCaps(Vec<PeriodFloorCap>),
}

impl Facet {
    /// Exactly one facet, parsed. See [`PatchPlanRequest`] for why one.
    fn of(body: PatchPlanRequest) -> Result<Self, DomainError> {
        let named = usize::from(body.shape.is_some())
            + usize::from(body.phases.is_some())
            + usize::from(body.addon_rules.is_some())
            + usize::from(body.descriptor_set.is_some())
            + usize::from(body.composites.is_some())
            + usize::from(body.period_floor_caps.is_some());
        if named != 1 {
            return Err(DomainError::InvalidRequest(format!(
                "a PATCH carries exactly one of `shape`, `phases`, `addon_rules`, \
                 `descriptor_set`, `composites` or `period_floor_caps`; this one carries \
                 {named}. Each of the six compare-and-swaps on the revision's own row version \
                 and advances it, so two in one request could not both satisfy one `If-Match`"
            )));
        }
        if let Some(shape) = body.shape {
            return Ok(Self::Shape(shape_patch(&shape)?));
        }
        if let Some(phases) = body.phases {
            return Ok(Self::Phases(
                phases.iter().map(phase_of).collect::<Result<_, _>>()?,
            ));
        }
        if let Some(rules) = body.addon_rules {
            let rules: Vec<AddonRule> = rules.into_iter().map(addon_of).collect();
            // The doors run here, at the parse, and not in the facet's arm in
            // `patch_plan`. That placement is the one D-342's phase door was
            // reordered for: the arm runs after `target_revision`, which on a plan
            // with no open draft *writes* — it opens a successor revision — so a
            // refusal there leaves a revision standing on a call the author was
            // told did not happen. Every operand is in the request, so nothing is
            // read to judge it.
            require_authorable_addon_bounds(&rules)?;
            require_distinct_addon_skus(&rules)?;
            return Ok(Self::AddonRules(rules));
        }
        if let Some(composites) = body.composites {
            let composites: Vec<CompositeMeter> =
                composites.into_iter().map(composite_of).collect();
            // The doors run here, at the parse, and not in the facet's arm in
            // `patch_plan`. That placement is the one D-342's phase door was
            // reordered for: the arm runs after `target_revision`, which on a plan
            // with no open draft *writes* — it opens a successor revision — so a
            // refusal there leaves a revision standing on a call the author was
            // told did not happen. Every operand is in the request, so nothing is
            // read to judge it.
            require_authorable_composites(&composites)?;
            return Ok(Self::Composites(composites));
        }
        if let Some(bounds) = body.period_floor_caps {
            let bounds: Vec<PeriodFloorCap> = bounds
                .iter()
                .map(period_floor_cap_of)
                .collect::<Result<_, _>>()?;
            // The write-stage door, here rather than in the facet's arm below, and
            // the placement is the one D-342's phase door was reordered for: the
            // arm runs after `target_revision`, which on a plan with no open draft
            // *writes* — it opens a successor revision — so a refusal there would
            // leave a revision standing on a call the author was told did not
            // happen. Every operand is in the request, so nothing is read to judge
            // it and it can run at the parse, which is where `shape_patch` keeps
            // its own door.
            require_authorable_period_bounds(&bounds)?;
            require_distinct_period_markets(&bounds)?;
            return Ok(Self::PeriodFloorCaps(bounds));
        }
        let set = body
            .descriptor_set
            .ok_or_else(|| DomainError::InvalidRequest("no facet named".to_owned()))?;
        Ok(Self::DescriptorSet(descriptor_of(set)))
    }
}

/// The revision a `PATCH` lands on, and the version the store will match.
struct PatchTarget {
    /// The revision number.
    revision: u64,
    /// The version the compare-and-swap is submitted against.
    expected: RowVersion,
}

/// Resolve the patch target: the open draft, or a successor opened from the
/// current revision.
///
/// # The tag names its subject, and both arms check it
///
/// `/plans/{planId}` does not name a revision, so a `PATCH` resolves one for
/// itself - and if the caller's tag carried only a version, that version would
/// be applied to whatever this resolution produced. The counters are unrelated:
/// a freshly opened successor stands at `RowVersion::new(0)`, exactly where the
/// plan's first draft stood, so a tag read against revision *N* would satisfy
/// the compare-and-swap on *N+1*. That is D-145's lost update arriving through
/// the **revision** instead of the version, and D-145 closed it on the storage
/// side precisely so a transport could not reopen it.
///
/// So both arms compare the tag's revision to the one they resolved: the
/// open-draft arm requires the tag to name that draft, and the successor arm
/// requires it to name the **current** revision (which is what the caller read)
/// before anything is opened. A mismatch is `STALE_VERSION` (409), the code the
/// design set already gives a caller working from a read it did not refresh.
///
/// # The successor arm's version check is a comparison, not a swap
///
/// `PlanRepo::open_revision` takes no `expected` version - nothing in the
/// design set gives it one, and the operation it performs is an insert rather
/// than an update, so there is no row for a compare-and-swap to match on. The
/// caller's `If-Match` is therefore tested here against the current revision's
/// row version, which leaves a **TOCTOU window** between that read and the
/// insert: a concurrent publish or retirement landing inside it is caught by
/// `open_revision`'s own refusals (`PLAN_RETIRED_NO_SUCCESSOR`,
/// `OPEN_DRAFT_REVISION_EXISTS`, and `uq_pricing_plan_open_draft` deciding the
/// race), but a concurrent edit that merely moved the current revision's tag is
/// not - the successor would open from a revision the caller had not read. This
/// is stated rather than hidden behind a comparison that looks atomic; closing
/// it needs an `expected` parameter on `open_revision`, which is a repository
/// change and a divergence this group reports rather than makes.
async fn target_revision(
    state: &AuthoringState,
    scope: &AccessScope,
    tenant: Uuid,
    plan_id: PlanId,
    asserted: RevisionTag,
    stamp: AuditStamp,
) -> Result<PatchTarget, CanonicalError> {
    let draft = state
        .plans
        .find_open_draft(scope, tenant, plan_id)
        .await
        .map_err(|e| CanonicalError::from(repo_failure(&e)))?;
    if let Some(draft) = draft {
        require_same_revision(plan_id, asserted, draft.revision).map_err(CanonicalError::from)?;
        return Ok(PatchTarget {
            revision: draft.revision,
            expected: asserted.version,
        });
    }

    let current = state
        .plans
        .find_current(scope, tenant, plan_id)
        .await
        .map_err(|e| CanonicalError::from(repo_failure(&e)))?;
    let Some(current) = current else {
        return Err(CanonicalError::from(
            spent_or_absent(&state.plans, scope, tenant, plan_id).await,
        ));
    };
    // The tag has to have been read from THIS revision, and to carry its
    // version. The first check is what stops a tag minted against a revision
    // that has since been superseded from opening a successor it never saw; the
    // second is the ordinary staleness test, taken here because the insert below
    // has no row to match it against.
    require_same_revision(plan_id, asserted, current.revision).map_err(CanonicalError::from)?;
    crate::domain::concurrency::require_match(current.row_version, asserted.version)
        .map_err(CanonicalError::from)?;

    let opened = state
        .plans
        .open_revision(scope, tenant, plan_id, stamp)
        .await
        .map_err(|e| CanonicalError::from(repo_failure(&e)))?;
    Ok(PatchTarget {
        revision: opened.revision,
        expected: opened.row_version,
    })
}

/// Refuse a precondition minted against a **different revision** of this plan.
///
/// `STALE_VERSION` rather than a new code, and it is the same refusal in
/// substance: the caller is working from a read it did not refresh, and what has
/// moved on is *which revision the plan offers for editing* rather than the
/// version of the one it offered before. The message names both revisions,
/// because that difference is the diagnosis - a caller one revision behind
/// published in between, and a caller naming a revision the plan never had is
/// pasting a tag from somewhere else.
pub(crate) fn require_same_revision(
    plan_id: PlanId,
    asserted: RevisionTag,
    resolved: u64,
) -> Result<(), DomainError> {
    if asserted.revision == resolved {
        return Ok(());
    }
    Err(DomainError::StaleVersion(format!(
        "plan {plan_id}: the precondition names revision {}, and the revision this call acts \
         on is {resolved}; re-read the plan and submit against the tag it answers",
        asserted.revision
    )))
}

/// The entity tag a plan route hands back: the revision, and its version.
fn plan_tag(view: &PlanView) -> String {
    preconditions::revision_etag(view.revision, RowVersion::new(view.row_version))
}

/// A plan with no current revision and no open draft: spent, or absent.
///
/// The discrimination S2 §5 owes this group. It is two reads, and the second one
/// is what keeps the refusal honest.
///
/// An empty chain is absent (404). A non-empty chain with no current revision
/// and no draft should be one whose every revision is `abandoned` — those are the
/// only states left once `draft`, `published` and `retired` are excluded, and a
/// `superseded` revision implies a successor that is current. **Should**, not
/// must: nothing in the schema forbids a chain whose greatest revision is
/// `superseded` with no successor, and telling an operator such a plan's "id is
/// spent, so mint a new plan" would be specific and false. So the greatest
/// revision's state is read, and a chain that is not tipped by a tombstone is
/// reported as the invariant breach it is rather than dressed as a lifecycle
/// refusal.
async fn spent_or_absent(
    plans: &PlanRepo,
    scope: &AccessScope,
    tenant: Uuid,
    plan_id: PlanId,
) -> DomainError {
    let highest = match plans.max_revision(scope, tenant, plan_id).await {
        Err(e) => return repo_failure(&e),
        Ok(None) => return not_readable(plan_id),
        Ok(Some(highest)) => highest,
    };
    match plans.find_revision(scope, tenant, plan_id, highest).await {
        Err(e) => repo_failure(&e),
        Ok(Some(row)) if row.lifecycle_state == LifecycleState::Abandoned => {
            DomainError::PlanAbandonedNoSuccessor(format!(
                "plan {plan_id} holds only abandoned revisions; its id is spent, so mint a \
                 new plan"
            ))
        }
        Ok(other) => DomainError::Internal(format!(
            "plan {plan_id} holds revision {highest} in state {}, with no current revision and \
             no open draft; the chain is unreachable and no refusal describes it",
            other.map_or_else(
                || "(absent)".to_owned(),
                |row| row.lifecycle_state.to_string()
            )
        )),
    }
}

/// Why a verb that needs the plan's open draft found none.
///
/// Three answers, and they are three because the operator's next action differs:
/// a spent plan is `PLAN_ABANDONED_NO_SUCCESSOR` (mint a new plan), a plan with
/// a current revision and no draft is `LIFECYCLE_FORBIDDEN` - S2 §5 in as many
/// words, and D-146's line about the code that holds refusals describing no
/// alternative action - and a plan with nothing at all is a 404.
///
/// **Shared with the publish mount**, which is why it takes a verb and a
/// repository rather than the abandon's state: S2 §5 gives `POST …/abandon` and
/// `POST …/publish` the same three refusals over the same missing subject, and
/// two spellings of one discrimination would be two chances for one of them to
/// answer a bare 404 for a plan whose id is spent.
pub(crate) async fn no_open_draft(
    plans: &PlanRepo,
    scope: &AccessScope,
    tenant: Uuid,
    plan_id: PlanId,
    verb: &str,
) -> DomainError {
    match plans.find_current(scope, tenant, plan_id).await {
        Err(e) => repo_failure(&e),
        Ok(Some(current)) => DomainError::LifecycleForbidden(format!(
            "plan {plan_id} holds no open draft revision to {verb}; its current revision {} \
             is {}",
            current.revision, current.lifecycle_state
        )),
        Ok(None) => spent_or_absent(plans, scope, tenant, plan_id).await,
    }
}

/// Read the revision back with its child sets and answer 200 + `ETag`.
///
/// A fresh read rather than the value the mutation returned, because a facet
/// mutation answers only the revision row while the response carries all four
/// facets - and because under a concurrent second edit the store's reading is
/// the only honest one.
async fn answer_revision(
    state: &AuthoringState,
    scope: &AccessScope,
    tenant: Uuid,
    plan_id: PlanId,
    revision: u64,
) -> Result<([(axum::http::HeaderName, String); 1], Json<PlanView>), CanonicalError> {
    let row = state
        .plans
        .find_revision(scope, tenant, plan_id, revision)
        .await
        .map_err(|e| CanonicalError::from(repo_failure(&e)))?
        .ok_or_else(|| CanonicalError::from(not_readable(plan_id)))?;
    let view = read_shape(state, scope, tenant, plan_id, revision, row)
        .await
        .map_err(|e| CanonicalError::from(repo_failure(&e)))?;
    Ok(([(ETAG, plan_tag(&view))], Json(view)))
}

/// The 201 a performed create answers with.
///
/// It takes the seeded phase rather than rendering an empty set, and the two
/// renderings are not interchangeable: this one is the **response**, while the
/// closure `guarded` was given renders the body it *stores* for a replay. A
/// create that answered `phases: []` here and the phase on the replay would hand
/// the first caller — the only one that ever sees a 201 — nothing to key its
/// price rows on, which is the whole of D-19's creation-time act missed by one
/// function.
fn created(revision: &PlanRevision, phase: &PlanPhase) -> Response {
    let view = PlanView::new(
        revision.clone(),
        revision.created_at_utc,
        vec![phase.clone()],
        Vec::new(),
        None,
        Vec::new(),
        Vec::new(),
    );
    let tag = plan_tag(&view);
    (
        StatusCode::CREATED,
        [
            (LOCATION, format!("{PLANS}/{}", revision.plan_id)),
            (ETAG, tag),
        ],
        Json(view),
    )
        .into_response()
}

/// One thing the clone left behind, on the wire.
///
/// A code and a count rather than a sentence: the operator's client renders it,
/// and a gear that shipped prose here would be deciding the wording for every
/// locale (`design/12-operator-efficiency.md` §3 `inst-cl-resets`, D-268).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct CloneNoticeView {
    /// What the clone did not carry across, or did without being asked —
    /// `no_coverage_scheduled`, `grandfathered_rows_not_copied`,
    /// `new_subscriptions_only_rows_not_copied`, or D-341's seeded terminal phase as
    /// `terminal_phase_adopted` / `terminal_phase_minted`.
    pub code: String,
    /// How many rows it covers. On `terminal_phase_adopted` the copied rows the seeded
    /// phase **attaches**; on `terminal_phase_minted` the copied rows it leaves
    /// **stranded**, which is every row that travelled.
    pub rows: usize,
}

/// What the clone produced and what it declined to produce.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct CloneReceiptView {
    /// The new plan, at revision `0` in `draft`.
    pub plan_id: Uuid,
    /// The plan it was cloned from — the same value the new row's `cloned_from`
    /// column carries (D-264).
    pub cloned_from: Uuid,
    /// Phase rows copied, each under a new `phase_id` (D-19). **Zero does not mean
    /// the clone holds no phase**: a source that held none has its terminal phase
    /// *seeded* rather than copied (D-341), which the notices name.
    pub phases_copied: usize,
    /// Price rows copied, each under a new `price_id` and a reset scope key.
    pub prices_copied: usize,
    /// Composite meters copied, each under a new `composite_id` (D-106).
    pub composites_copied: usize,
    /// **What the operator would otherwise learn from a refused publish.** The
    /// clone's billable rows have no window coverage (`inst-cl-windows`), both
    /// cutover-made eligibility classes stay behind (D-268), and a phase-less source
    /// has its terminal phase seeded — adopted from the copied rows, or minted with
    /// those rows left stranded (D-341). Each is reported here, counted per class,
    /// rather than discovered later.
    pub notices: Vec<CloneNoticeView>,
}

impl From<CloneReceipt> for CloneReceiptView {
    fn from(receipt: CloneReceipt) -> Self {
        Self {
            plan_id: receipt.plan_id.get(),
            cloned_from: receipt.cloned_from.get(),
            phases_copied: receipt.phases_copied,
            prices_copied: receipt.prices_copied,
            composites_copied: receipt.composites_copied,
            notices: receipt.notices.iter().map(notice_view).collect(),
        }
    }
}

/// One notice as a code and a count.
///
/// Matched exhaustively rather than with a catch-all, so a fourth class added to
/// [`CloneNotice`] has to be given a wire code here instead of vanishing from the
/// response — which is the failure mode a notice exists to prevent.
fn notice_view(notice: &CloneNotice) -> CloneNoticeView {
    let (code, rows) = match notice {
        CloneNotice::NoCoverageScheduled { rows } => ("no_coverage_scheduled", *rows),
        CloneNotice::GrandfatheredRowsNotCopied { rows } => {
            ("grandfathered_rows_not_copied", *rows)
        }
        CloneNotice::NewSubscriptionsOnlyRowsNotCopied { rows } => {
            ("new_subscriptions_only_rows_not_copied", *rows)
        }
        // **Two codes for one variant, because the two outcomes are what a client
        // acts on** (D-341): under `Adopted` the
        // copied rows are attached and the clone is publishable once windows are
        // scheduled; under `Minted` every copied row is stranded and the draft cannot
        // publish until a human decides which id the plan holds. A single code with
        // the origin in a second field would make the milder case and the blocking
        // one indistinguishable to a consumer matching on `code`, which is what §3.3
        // says a code is for.
        CloneNotice::TerminalPhaseSeeded { origin, rows } => (
            match origin {
                SeededPhaseOrigin::Adopted => "terminal_phase_adopted",
                SeededPhaseOrigin::Minted => "terminal_phase_minted",
            },
            *rows,
        ),
    };
    CloneNoticeView {
        code: code.to_owned(),
        rows,
    }
}

/// `POST /plans/{planId}/clone`.
///
/// # The source is named and the target is minted
///
/// The path names the plan being copied; the **new** plan's id is minted here,
/// inside the guarded body, for `create_plan`'s reason exactly: an id minted
/// outside the claim is an id a replay would answer differently, and the whole
/// point of storing the response body is that the second caller learns the
/// *first* caller's plan.
///
/// # One scope, gated on the source, and D-279 is why it is not two
///
/// A compiled [`AccessScope`] is the authorization answer **and** the `SecureORM`
/// row filter, and this route reads plan A while writing plan B — so D-278 split
/// it in two, a source scope for the reads and `create_plan`'s resource-less one
/// for the writes. **That remedy was wrong and is reverted.** The child tables
/// bind `RESOURCE_ID` to their own id, not to `plan_id`, so a scope naming a
/// *plan* filters `plan_phase`, `pricing_price`, `composite_meter` and
/// `pricing_bundle` to zero rows; a separate source scope could not read the
/// source's own children, and the clone became a silently empty plan where it had
/// been a loud denial.
///
/// One scope, `plan x write` with `resource_id = Some(source)` — the question
/// actually being asked, may this principal make a plan out of *this* one — and
/// `owner_tenant_id` the caller's tenant, so `access_scope`'s membership
/// assertion refuses a source outside the compiled scope rather than trusting the
/// PDP filtered it. Under an id-shaped answer the clone is refused at its first
/// write, which is correct: this gear cannot express "that plan's subtree" as a
/// `RESOURCE_ID` constraint on any route, and failing closed beats writing half a
/// plan.
///
/// # What the digest covers, and why it is not the empty body
///
/// The request carries no body, so the payload hash is taken over the **source
/// plan id**. Without it every clone in a tenant would hash identically, and a
/// client reusing a key against a different source would be answered the first
/// clone's plan instead of `IDEMPOTENCY_PAYLOAD_MISMATCH` — a replay of an act
/// nobody performed.
///
/// # No `If-Match`
///
/// The clone reads the source's *current* revision and writes nothing to it, so
/// there is no version of the source to hold. Its siblings on this path take a
/// precondition because they mutate the revision the tag names; this one does
/// not mutate the source at all.
async fn clone_plan(
    Extension(state): Extension<Arc<AuthoringState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    extension_correlation: Option<Extension<CorrelationId>>,
    Path(plan_id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let correlation = require_correlation(extension_correlation)?;
    let tenant = ctx.subject_tenant_id();
    let source = PlanId::new(plan_id);
    let scope = write_scope(&enforcer, &ctx, source.get(), tenant).await?;

    let client_key = preconditions::idempotency_key(&headers)?;
    let request_hash =
        preconditions::request_digest(&serde_json::json!({ "source_plan_id": source.get() }))?;
    let now = OffsetDateTime::now_utc();
    let stamp = audit_stamp(&ctx, now, correlation);

    let guard = GuardedRequest {
        operation: CLONE_PLAN_OPERATION,
        client_key,
        request_hash,
        tenant_id: tenant,
        status: StatusCode::CREATED.as_u16().into(),
        now,
    };
    let scope_for_body = scope.clone();
    let outcome = idempotent::guarded(
        &state.db,
        &state.idempotency,
        &scope,
        guard,
        move |txn: &DbTx<'_>| -> TxFuture<'_, CloneReceipt> {
            Box::pin(async move {
                // The gate owns the transaction and the clone joins it — which is
                // the whole of D-275: the nine-writer copy either commits with
                // the idempotency claim or leaves nothing, and there is no form of
                // the cloner that could open a second transaction here.
                Box::pin(clone_plan_on(
                    txn,
                    &scope_for_body,
                    tenant,
                    source,
                    PlanId::new(Uuid::now_v7()),
                    now,
                    stamp,
                ))
                .await
            })
        },
        // The receipt IS the recorded body, so a replay answers the first
        // caller's plan id and its notices verbatim — which is the only way the
        // second caller learns that the class it is missing was left behind on
        // purpose rather than lost by the retry.
        |receipt: &CloneReceipt| {
            serde_json::to_value(CloneReceiptView::from(receipt.clone()))
                .map_err(|e| DomainError::Internal(format!("cannot render the clone receipt: {e}")))
        },
    )
    .await
    .map_err(CanonicalError::from)?;

    Ok(match outcome {
        Guarded::Performed(receipt) => cloned(receipt),
        Guarded::Replayed { status, body } => replayed(CLONE_PLAN_OPERATION, status, &body)?,
    })
}

/// `201` with the new plan's location and the receipt.
///
/// No `ETag`: the clone answers a *receipt*, not a revision, and a tag on a body
/// that is not the resource is a precondition token pointing at nothing. A caller
/// that wants the draft's tag reads it from `Location`, which is the read that
/// would have to happen anyway before an edit.
fn cloned(receipt: CloneReceipt) -> Response {
    let view = CloneReceiptView::from(receipt);
    let location = format!("{PLANS}/{}", view.plan_id);
    (StatusCode::CREATED, [(LOCATION, location)], Json(view)).into_response()
}

/// The recorded answer a replay is handed back, verbatim.
///
/// `Location` is rebuilt from the **recorded body**, which is a pure function of
/// an id that body carries. No `ETag`: the dedup row stores a status and a body
/// and no headers, and the revision's row version may have moved since the
/// record was written - a tag rebuilt from a stale body would be a precondition
/// token that looks valid and is not.
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
    let location = body
        .get("plan_id")
        .and_then(serde_json::Value::as_str)
        .map(|plan_id| format!("{PLANS}/{plan_id}"));
    Ok(match location {
        Some(location) => (status, [(LOCATION, location)], Json(body.clone())).into_response(),
        None => (status, Json(body.clone())).into_response(),
    })
}

// ---------------------------------------------------------------------------
// Wire -> domain. Every one of these refuses rather than coerces: a token the
// domain does not know is a request this gear cannot interpret, which is the
// Foundation validation envelope's own case and mints no code.
// ---------------------------------------------------------------------------

/// A create's shape, parsed, and still missing the five things only the handler
/// knows: the minted id, the tenant, the actor, the authoring instant and the
/// request's correlation.
struct DraftShape {
    /// The catalog SKU this plan realizes.
    sku_id: Option<Uuid>,
    /// The plan's tier.
    plan_tier: Option<String>,
    plan_name: Option<String>,
    /// The plan's billing cycle.
    billing_cycle: Option<BillingCycle>,
    /// The recurring frequency, interval and all.
    frequency: Option<Frequency>,
    /// The audited tier override (P3); absent means no override, because the
    /// column is `NOT NULL DEFAULT false` and "nobody said" and "no override"
    /// are the same claim about a plan.
    plan_tier_override: bool,
    /// Minimum purchasable quantity.
    purchase_min_qty: Option<u64>,
    /// Maximum purchasable quantity.
    purchase_max_qty: Option<u64>,
    /// The Billing invoice-layout hint.
    invoice_grouping_key: Option<String>,
    /// Start of the availability window.
    available_from: Option<OffsetDateTime>,
    /// End of the availability window.
    available_to: Option<OffsetDateTime>,
    /// The two governed facets the create accepts and now stores: the entitlement
    /// grant set and the plan-change contract.
    ///
    /// Both were **parsed, hashed into the idempotency digest and dropped** before
    /// a review, because `NewPlanDraft` has no field for either and
    /// nothing on the create path carried them. They travel as one value because
    /// `plan_repo::AuthoredGrants` is what the repository takes them as, and because
    /// a create authors both or neither.
    granted: plan_repo::AuthoredGrants,
}

impl DraftShape {
    /// Bind the shape to the identity and provenance the surface supplies.
    ///
    /// `created_at_utc` is the request's instant rather than the database's:
    /// §2.2 makes the catalog author-driven, so the authoring instant belongs to
    /// the call and not to whichever node evaluated `now()`.
    ///
    /// It answers the draft **and** the two governed facets, because they are written
    /// by one call and by different fields of one row: `NewPlanDraft` is the columns
    /// every create path shares and [`plan_repo::AuthoredGrants`] is the pair only an
    /// authored create can supply. The two types are unmistakable for each other at
    /// the one call site, which is why they are a pair rather than a third struct.
    fn into_draft(
        self,
        plan_id: PlanId,
        tenant_id: Uuid,
        created_by: Uuid,
        created_at_utc: OffsetDateTime,
        correlation_id: Uuid,
    ) -> (NewPlanDraft, plan_repo::AuthoredGrants) {
        let granted = self.granted;
        let draft = NewPlanDraft {
            plan_id,
            tenant_id,
            created_by,
            created_at_utc,
            sku_id: self.sku_id,
            plan_tier: self.plan_tier,
            plan_name: self.plan_name,
            billing_cycle: self.billing_cycle,
            frequency: self.frequency,
            plan_tier_override: self.plan_tier_override,
            purchase_min_qty: self.purchase_min_qty,
            purchase_max_qty: self.purchase_max_qty,
            invoice_grouping_key: self.invoice_grouping_key,
            available_from: self.available_from,
            available_to: self.available_to,
            // The authoring surface never sets lineage: `POST /plans` creates an
            // authored plan by definition, and a caller able to claim a source
            // could forge provenance for a plan it never copied. The clone path
            // is the only writer (`inst-cl-copy`).
            cloned_from: None,
            correlation_id,
        };
        (draft, granted)
    }
}

/// The write-stage door for the plan name (D-318).
///
/// **The plan write path runs no rule pipeline**, unlike the price row's, whose
/// `require_no_key_contradiction` runs `price_row_rules()` and takes
/// `write_stage_only()`. Turning the whole plan pipeline on here would refuse
/// legitimate intermediate states — `PlanTierDeclared` alone would make a draft
/// with no tier yet unsaveable — which is the mistake D-312's stage split exists
/// to avoid. So exactly one rule's predicate is applied, the one whose every
/// operand is in the request and which guards a column that must never hold
/// `''`.
///
/// It answers the enumerated report shape rather than a bare message, so a
/// client already parsing publish violations parses this unchanged.
fn require_well_formed_plan_name(name: Option<&str>) -> Result<(), DomainError> {
    let Some(name) = name else { return Ok(()) };
    let Some(detail) = crate::domain::plan_rules::composition::plan_name_fault(name) else {
        return Ok(());
    };
    let mut report = crate::domain::validation::ValidationReport::default();
    report.violate_at_write(
        crate::domain::plan_rules::PLAN_NAME_INVALID,
        "plan".to_owned(),
        detail,
    );
    Err(DomainError::ValidationFailed(without_placeholder_plan(
        report,
    )))
}

/// The write-stage door for the phase set (**D-342**).
///
/// `inst-ph-row-attached` reported through `violate`, which defaults to the Publish
/// stage, so this facet accepted `{"phases": []}` and any replace dropping a phase
/// a draft price row names — and the facet is a **wholesale replace**, deleting the
/// revision's phase rows and re-inserting the payload, so the stranding is one
/// successful `PATCH`. The author learned at publish, which is the single moment at
/// which their options are strictly fewer: at the write there are two remedies —
/// keep the phase, or drop the rows deliberately — while at publish a published row
/// cannot be re-pointed at all (a scope key is the row's identity, Foundation §3.7)
/// and a draft row can only be deleted.
///
/// **Three rules, not the pipeline**, which is the distinction
/// [`require_well_formed_plan_name`] states above: turning the whole plan pipeline
/// on here would refuse a draft for having no tier, no frequency and no descriptor
/// set yet — legitimate intermediate states, and the mistake D-312's stage split
/// exists to avoid. The three applied are the ones whose every operand is in hand
/// at this door: the submitted phase set **is** the request, and the rows are
/// already stored.
///
/// - `inst-ph-row-attached` (D-342), the stranding.
/// - `inst-ph-graph`, the phase set judged against itself. It
///   owns "exactly one terminal phase", and the at-most-one half was reaching
///   `uq_pricing_plan_phase_terminal` instead — a caller-fixable payload answered
///   `500 … please retry later`, which is the class D-340 filed as `[H]` about the
///   *other* unique constraint on the same statement.
/// - `inst-ph-trial`, one arm of it: a phase whose
///   `displayTrialDays` and `phaseDurationDays` are both set and disagree. That was
///   reaching `chk_pricing_plan_phase_display_trial_days` and answering the same
///   `500`. It is the **only** rule of the three that takes no stage parameter —
///   the arm stamps itself `violate_at_write` and its two siblings do not — so the
///   filter below is what keeps the other two at publish, where D-151 wants them:
///   a non-trial `kind` and a missing duration are states the store accepts, and
///   refusing them here would refuse a half-authored trial phase.
///
/// The remaining siblings stay out, and that is the boundary rather than an omission:
/// `PhaseChainLinear` and `TerminalPhaseKind` read the same submitted set, but a
/// facet that refused a non-evergreen terminal or a non-linear ordinal run at the
/// write would be a wider refusal than the review measured a fault for, and the
/// direction that costs an author nothing is to add one when one is measured.
///
/// **The rows are the same operand the publish stage judges** —
/// `publish::CANDIDATE_ROW_STATES`, the published plane plus the drafts this
/// revision would publish — read through the same repository call
/// `publish::assemble_from` makes. A door judging a different row set from the gate
/// behind it is how a write-time check starts disagreeing with the publish it is
/// supposed to anticipate.
///
/// **The subject carries only what the rule reads**, and that is stated rather than
/// left to be discovered: `RowPhaseAttached` reads `rows`, `phases` and the
/// `plan_id`/`revision` its finding is filed under. The `revision` is a **label and
/// not an operand** — the rows are read plan-wide — which is what lets this door run
/// before the revision the patch lands on has been resolved, and why the caller
/// hands it the revision the `If-Match` names: the one the author read. On the
/// open-draft arm that is the landing revision; on the successor arm it is that
/// revision's predecessor, and a finding labelled with a successor number the caller
/// has never seen would locate the fault no better. The rest of [`PlanShape`](crate::domain::plan_shape::PlanShape) stays
/// at its default, which is safe in the one direction that matters — publish
/// re-judges the whole assembled shape, so a field this door leaves unloaded can
/// only delay a refusal, never lose one. Assembling the full shape here would cost
/// six reads on every phase edit to answer a question two of them settle.
///
/// **Not a guarantee, and not claimed as one.** The read is not the write's guard:
/// a price row authored between this check and the replace is judged at publish,
/// which is why D-342 keeps the publish-stage registration rather than moving the
/// rule. The two facets are authored through different calls and publish remains
/// the gate every writer passes through.
///
/// It answers the enumerated report shape, so a client already parsing publish
/// violations parses this unchanged.
async fn require_no_stranded_rows(
    state: &AuthoringState,
    scope: &toolkit_db::secure::AccessScope,
    tenant: Uuid,
    plan_id: PlanId,
    revision: u64,
    phases: &[PlanPhase],
) -> Result<(), CanonicalError> {
    let conn = state.db.conn().map_err(|e| {
        CanonicalError::from(DomainError::Internal(format!("phase door conn: {e}")))
    })?;
    let rows = crate::infra::storage::repo::price_repo::load_for_plan(
        &conn,
        scope,
        tenant,
        plan_id,
        crate::infra::publish::CANDIDATE_ROW_STATES,
    )
    .await
    .map_err(|e| CanonicalError::from(repo_failure(&e)))?;

    let mut subject =
        crate::domain::plan_shape::PlanShape::new(plan_id, revision, OffsetDateTime::now_utc());
    subject.phases = crate::domain::plan_shape::PhaseGraph::new(phases.to_vec());
    subject.rows = rows;

    let write = crate::domain::validation::Stage::Write;
    let mut report = crate::domain::validation::ValidationReport::default();
    // The stranding first, and the order is the one thing about this pair that is
    // observable: a payload that both strands rows and holds no terminal phase — the
    // empty set — reports both, and a client reading the *first* violation of the
    // envelope reads the one that names the rows. That is the fault the author acts
    // on; the graph fault is a consequence of the same empty payload.
    crate::domain::validation::ValidationRule::evaluate(
        &crate::domain::plan_rules::phase_graph::RowPhaseAttached { stage: write },
        &subject,
        &mut report,
    );
    // `inst-ph-graph`. Its at-most-one-terminal half
    // was reaching `uq_pricing_plan_phase_terminal` and coming back a `500` advising
    // a retry — the class D-340 filed as [H], on the sibling constraint of the same
    // statement. The rule owns "exactly one terminal phase", so it refuses it here
    // with its own message and the store constraint becomes a backstop; a second
    // typed code at the store would have made two owners of one requirement.
    crate::domain::validation::ValidationRule::evaluate(
        &crate::domain::plan_rules::phase_graph::PhaseGraphIntegrity { stage: write },
        &subject,
        &mut report,
    );
    // `inst-ph-trial`. Its drift arm — two numbers
    // set and disagreeing — was reaching `chk_pricing_plan_phase_display_trial_days`
    // and coming back a `500` advising a retry, the same class as the terminal-count
    // fault above. It needs no stage parameter, and that is the difference from its
    // two neighbours here: only the drift arm is `violate_at_write`, so the filter
    // below keeps exactly the fault the store would have refused and leaves the
    // rule's two publish-stage arms — both of them legitimate intermediate drafts —
    // where D-151 put them.
    crate::domain::validation::ValidationRule::evaluate(
        &crate::domain::plan_rules::phase_graph::DisplayTrialDaysOnTrialPhase,
        &subject,
        &mut report,
    );
    // The third caller of `write_stage_only()`, after the price-row write and the
    // bulk import. It is not decorative on a report this door built: it is the
    // guarantee that only a write-judgeable finding can refuse a write, so a
    // publish-stage arm added to either rule later cannot leak into this refusal
    // without somebody stating its stage.
    match report.write_stage_only() {
        None => Ok(()),
        Some(write_stage) => Err(CanonicalError::from(DomainError::ValidationFailed(
            write_stage,
        ))),
    }
}

/// Parse a create's shape.
fn shape_of(body: &PlanShapeRequest) -> Result<DraftShape, DomainError> {
    require_well_formed_plan_name(body.plan_name.as_deref())?;
    require_authorable_purchase_window(body.purchase_min_qty, body.purchase_max_qty)?;
    require_ordered_availability_window(body.available_from, body.available_to)?;
    Ok(DraftShape {
        sku_id: body.sku_id,
        plan_tier: body.plan_tier.clone(),
        plan_name: body.plan_name.clone(),
        billing_cycle: billing_cycle_of(body.billing_cycle.as_deref())?,
        frequency: frequency_of(body.frequency.as_ref())?,
        plan_tier_override: body.plan_tier_override.unwrap_or(false),
        purchase_min_qty: body.purchase_min_qty,
        purchase_max_qty: body.purchase_max_qty,
        invoice_grouping_key: body.invoice_grouping_key.clone(),
        available_from: body.available_from,
        available_to: body.available_to,
        // The two members this parse used to skip. Read through the same two
        // functions `shape_patch` reads them through, so the create and the patch
        // cannot come to disagree about what a grant set on the wire means.
        granted: plan_repo::AuthoredGrants {
            entitlement_grants: body
                .entitlement_grants
                .as_ref()
                .map(read_grants)
                .unwrap_or_default(),
            change_contract: body
                .change_contract
                .as_ref()
                .map(read_change_contract)
                .transpose()?
                .unwrap_or_default(),
        },
    })
}

/// Parse a patch's shape facet.
///
/// **Nine of these fields cannot be *cleared* through a `PATCH`**, because
/// [`PlanShapePatch`] carries a single `Option` per field and an omitted member
/// and an explicit `null` are therefore the same value. The double option that
/// would separate them is a change to the domain patch type and to
/// `plan_repo::patched_columns`, not to this surface - the surface is correct
/// without it - and it is reported rather than approximated here. What a caller
/// can do meanwhile is replace a value, or discard the whole draft revision
/// through `POST …/abandon`, which keeps the revision number it consumed
/// (D-145).
fn shape_patch(body: &PlanShapeRequest) -> Result<PlanShapePatch, DomainError> {
    require_well_formed_plan_name(body.plan_name.as_deref())?;
    require_authorable_purchase_window(body.purchase_min_qty, body.purchase_max_qty)?;
    require_ordered_availability_window(body.available_from, body.available_to)?;
    Ok(PlanShapePatch {
        sku_id: body.sku_id,
        plan_tier: body.plan_tier.clone(),
        plan_name: body.plan_name.clone(),
        billing_cycle: billing_cycle_of(body.billing_cycle.as_deref())?,
        frequency: frequency_of(body.frequency.as_ref())?,
        plan_tier_override: body.plan_tier_override,
        purchase_min_qty: body.purchase_min_qty,
        purchase_max_qty: body.purchase_max_qty,
        invoice_grouping_key: body.invoice_grouping_key.clone(),
        available_from: body.available_from,
        available_to: body.available_to,
        entitlement_grants: body.entitlement_grants.as_ref().map(read_grants),
        change_contract: body
            .change_contract
            .as_ref()
            .map(read_change_contract)
            .transpose()?,
    })
}

/// Read the wire grant set into the domain value.
///
/// Nothing is refused here: the shape carries no closed vocabulary, and the one
/// check the design states — that every per-phase key names a phase of the
/// plan's own schedule — is a publish rule (`inst-gs-perphase`), because the
/// phase schedule is a *different* facet of the same revision and a `PATCH` may
/// legitimately arrive before it.
fn read_grants(request: &EntitlementGrantsRequest) -> EntitlementGrants {
    EntitlementGrants {
        plan_tier_ref: request.plan_tier_ref.clone(),
        plan_level: GrantSet {
            feature_flags: request.feature_flags.clone().unwrap_or_default(),
            quotas: request.quotas.clone().unwrap_or_default(),
        },
        per_phase: request
            .per_phase
            .clone()
            .unwrap_or_default()
            .into_iter()
            .map(|(id, set)| {
                (
                    id,
                    GrantSet {
                        feature_flags: set.feature_flags.unwrap_or_default(),
                        quotas: set.quotas.unwrap_or_default(),
                    },
                )
            })
            .collect(),
    }
}

/// Read the wire plan-change contract into the domain value.
///
/// The only thing refused here is a token outside D-113's pair. The edge list
/// and the rank are judged at **publish** rather than at this boundary, because
/// a draft is allowed to be unfinished and because `inst-pc-targets` needs the
/// published-plan lookup this surface does not hold — the same division
/// `inst-pi-required` draws for the proration set.
fn read_change_contract(
    request: &PlanChangeContractRequest,
) -> Result<PlanChangeContract, DomainError> {
    Ok(PlanChangeContract {
        allowed_change_targets: request.allowed_change_targets.clone(),
        comparability_rank: request.comparability_rank,
        usage_counter_on_plan_change: match request.usage_counter_on_plan_change.as_deref() {
            // Omitted is D-113's ratified default, not a missing field: absence
            // **is** `reset`, so there is nothing to refuse.
            None => UsageCounterOnPlanChange::Reset,
            Some(token) => UsageCounterOnPlanChange::ALL
                .iter()
                .copied()
                .find(|candidate| candidate.as_str() == token)
                .ok_or_else(|| {
                    DomainError::InvalidRequest(format!(
                        "change_contract.usage_counter_on_plan_change `{token}` is not one of \
                         reset, carry"
                    ))
                })?,
        },
    })
}

/// Parse a billing-cycle token.
fn billing_cycle_of(raw: Option<&str>) -> Result<Option<BillingCycle>, DomainError> {
    raw.map(|token| {
        wire_token(
            "billing_cycle",
            token,
            BillingCycle::ALL,
            BillingCycle::as_str,
        )
    })
    .transpose()
}

/// Parse a frequency, interval included.
///
/// The interval is required on the custom variant and refused on every other, so
/// the pairing `Frequency` cannot represent is one this surface cannot accept
/// either - rather than being discovered by a `CHECK` and reported as a storage
/// failure.
fn frequency_of(raw: Option<&FrequencyView>) -> Result<Option<Frequency>, DomainError> {
    let Some(view) = raw else {
        return Ok(None);
    };
    let kind = wire_token(
        "frequency.kind",
        &view.kind,
        Frequency::ALL,
        Frequency::as_str,
    )?;
    match kind {
        Frequency::CustomEveryN { .. } => {
            let (Some(n), Some(unit)) =
                (view.custom_interval_n, view.custom_interval_unit.as_deref())
            else {
                return Err(DomainError::InvalidRequest(
                    "frequency `custom_every_n` carries `custom_interval_n` and \
                     `custom_interval_unit`; a token alone does not say what interval was \
                     authored"
                        .to_owned(),
                ));
            };
            Ok(Some(Frequency::CustomEveryN {
                n,
                unit: wire_token(
                    "frequency.custom_interval_unit",
                    unit,
                    CustomIntervalUnit::ALL,
                    CustomIntervalUnit::as_str,
                )?,
            }))
        }
        fixed => {
            if view.custom_interval_n.is_some() || view.custom_interval_unit.is_some() {
                return Err(DomainError::InvalidRequest(format!(
                    "frequency `{}` carries no interval; the interval belongs to \
                     `custom_every_n` alone",
                    view.kind
                )));
            }
            Ok(Some(fixed))
        }
    }
}

/// Parse one phase.
fn phase_of(view: &PlanPhaseView) -> Result<PlanPhase, DomainError> {
    Ok(PlanPhase {
        phase_id: PhaseId::new(view.phase_id),
        kind: wire_token("phases.kind", &view.kind, PhaseKind::ALL, PhaseKind::as_str)?,
        // A blank label is an absence wearing a value's shape (D-318's reading of
        // `planName`): `""` and `"   "` read as no label, never as a label.
        display_name: view
            .display_name
            .as_deref()
            .map(str::trim)
            .filter(|label| !label.is_empty())
            .map(ToOwned::to_owned),
        ordinal: view.ordinal,
        converts_to_phase_id: view.converts_to_phase_id.map(PhaseId::new),
        phase_duration_days: view.phase_duration_days,
        display_trial_days: view.display_trial_days,
    })
}

/// Parse one add-on rule. Nothing here can fail: every member is already the
/// type the domain holds, and the rules that judge them run at publish.
fn addon_of(view: AddonRuleView) -> AddonRule {
    AddonRule {
        addon_sku_id: view.addon_sku_id,
        required: view.required,
        min_qty: view.min_qty,
        max_qty: view.max_qty,
        step_qty: view.step_qty,
        price_override_ref: view.price_override_ref,
        depends_on: view.depends_on,
        conflicts_with: view.conflicts_with,
    }
}

/// Parse one composite meter.
///
/// Nothing here can fail, and that is a statement about where the rules live
/// rather than an absence of them: `output_unit`'s non-emptiness is a `CHECK`,
/// arity and self-reference are publish rules over the **whole revision's** set,
/// and the formula is opaque to this crate by A4. A refusal here would have to
/// be one no document declares.
///
/// Not because "a self-reference cycle spans two definitions and no single one of
/// them is wrong": `CompositeSelfReference` refuses the direct case too, and says
/// so in as many words, with a case pinning it. The reason is the one above — an
/// author assembles a revision's set over several calls, so judging it at save
/// time would refuse an intermediate state the design expects (D-304).
///
/// **What can fail here is the store, and this doc did not say so.** The
/// paragraph that stood here named two such faults and said both were "pinned in
/// `rest_plans.rs` rather than fixed, which is `pricing_plan_phase`'s posture one
/// table over". One of the two is now fixed and that posture no longer exists:
///
/// - A **pasted `composite_id`** was a `500` because the primary key
///   `(composite_id, plan_revision)` carried neither `plan_id` nor `tenant_id`
///   while the id is client-supplied, so an id another tenant held at the same
///   revision number collided. The key is `(tenant_id, plan_id, plan_revision,
///   composite_id)` — D-340's shape, one table over from `pricing_plan_phase`'s — and
///   the collision it leaves is the one that is genuinely the caller's: the same id
///   twice inside one revision.
/// - A **repeated `output_unit`** in one payload still reaches the caller as a
///   bare `500` through `uq_pricing_composite_meter_output`, which is correctly
///   tenant-scoped and is an ordinary duplicate-input mistake. It has no isolation
///   dimension and is pinned rather than fixed; the posture that
///   fits it is `names_the_phase_key`'s — refuse at the door over the submitted
///   set — rather than a second typed code from the repository.
///
/// The id is minted when the author omits it - see
/// [`CompositeMeterRequest::composite_id`] for why the surface mints rather than
/// requiring one. `now_v7` matches every other id this surface produces.
fn composite_of(view: CompositeMeterRequest) -> CompositeMeter {
    CompositeMeter {
        composite_id: view.composite_id.unwrap_or_else(Uuid::now_v7),
        // **Stored as the door judged it.** `require_authorable_composites` trims
        // before it decides blankness and duplication, and storing the untrimmed
        // spelling made the normalisation the door judges by one the store never
        // sees: `{"outputUnit": " mb"}` passed and was persisted with the space,
        // and `CompositeSelfReference` keys its edge map on the exact stored string
        // and compares constituents with `==` — so a real two-composite cycle
        // (`gb` from `mb`, ` mb` from `gb`) produced no edge, was not reported, and
        // published as a formula defined in terms of itself. Normalising once, here,
        // is what `price_record::canonical_usage_line` already does for the
        // meter/dimension pair, and `chk_pricing_composite_meter_output_unit` now
        // carries the matching `btrim`/`trim` predicate the five taxonomies use.
        output_unit: view.output_unit.trim().to_owned(),
        constituent_units: view
            .constituent_units
            .into_iter()
            .map(|unit| unit.trim().to_owned())
            .collect(),
        // An omitted formula is JSON `null` rather than an error: the store's
        // column is `NOT NULL` and holds `jsonb`, so `null` is a value it accepts,
        // and A4 puts the formula's meaning outside this crate entirely.
        formula: view.formula.unwrap_or(serde_json::Value::Null),
    }
}

/// Parse one period floor/cap (**D-319**).
///
/// Three things can fail here, and all three are **shape** faults rather than
/// policy: the currency is not ISO 4217, the region axis value is blank, or an
/// amount is negative.
///
/// One policy question is judged at publish — a market the plan does not sell,
/// whose operand is the plan's whole row set — and **the amount questions are
/// not**. Judging a `0` bound or a floor above its cap at publish is unreachable in
/// both directions: the four `CHECK`s on `pricing_plan_period_floor_cap` refuse the
/// row before any publish sees it, so the author gets neither a report line nor a
/// 400 but a `500` carrying a constraint name, and
/// `PERIOD_FLOOR_CAP_AMOUNT_INVALID` has no raiser at all. They are judged by
/// [`require_authorable_period_bounds`], one call up, which reports the same rule's
/// findings enumerated — the market **is** named, in a 400.
fn period_floor_cap_of(view: &PeriodFloorCapView) -> Result<PeriodFloorCap, DomainError> {
    Ok(PeriodFloorCap {
        currency: CurrencyCode::new(&view.currency)?,
        region: Region::new(&view.region)?,
        floor_minor: view.floor_minor.map(MinorAmount::new).transpose()?,
        cap_minor: view.cap_minor.map(MinorAmount::new).transpose()?,
    })
}

/// Parse a descriptor set.
fn descriptor_of(view: DescriptorSetView) -> DescriptorSet {
    DescriptorSet {
        invoice_line_template: view.invoice_line_template,
        gl_code: view.gl_code,
        itemization_rule: view.itemization_rule,
        additional: view.additional,
    }
}

/// Read a wire token back into the domain value that renders it.
///
/// The candidate list is always the enum's own `ALL`, never a copy: a
/// hand-written table is one a variant added later goes missing from while
/// everything still compiles, and what that produces is a perfectly legal
/// request refused as unknown.
fn wire_token<T: Copy>(
    field: &str,
    token: &str,
    candidates: &[T],
    render: fn(T) -> &'static str,
) -> Result<T, DomainError> {
    candidates
        .iter()
        .copied()
        .find(|candidate| render(*candidate) == token)
        .ok_or_else(|| {
            let known: Vec<&str> = candidates.iter().copied().map(render).collect();
            DomainError::InvalidRequest(format!(
                "{field} `{token}` is not one of {}",
                known.join(", ")
            ))
        })
}

#[cfg(test)]
#[path = "plans_tests.rs"]
mod plans_tests;
