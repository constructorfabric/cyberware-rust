//! The price authoring plane: `POST`, `GET` by id, `PATCH`, `DELETE` and the paginated list
//! (`design/03-price-structure.md` §5, D-125, D-140, D-141, D-148).
//!
//! # This surface validates a row's shape only where the frozen key already
//! # decides it, and a reader would otherwise assume either extreme
//!
//! `MODEL_KIND_MISSING`, the tier-band geometry family, `AMOUNT_PLACEMENT_INVALID`,
//! `LEVEL_*` and most of Slice 3's rules are **publish-time** and do not run here.
//! Authoring a shape-invalid draft stays legal: §4.2 puts the whole rule set at the
//! publish pre-check, an author assembles a row over several calls, and refusing an
//! intermediate state at save time would contradict that design outright.
//!
//! **D-312 carves out one narrow subset, and it is a subset of violations rather
//! than of rules.** `POST` and `PATCH` run the same row rule set and keep only what
//! is marked [`Stage::Write`](crate::domain::validation::Stage): a fault whose
//! every operand is present in the request, so the row is complete,
//! self-inconsistent and knowably unpublishable the instant it arrives and
//! refusing it here costs an author nothing they could legitimately have wanted.
//! The strongest case is a fault one of whose operands is frozen by the canonical
//! scope key — `chargeKind` above all, the key being uneditable after create —
//! because then the only call that resolves it *retracts the field just sent*
//! rather than adding anything.
//!
//! **A frozen-key operand is not required for the write stage.** D-312's amendment
//! puts both arms of `inst-mk-forbidden` at the write — tier bands on a `flat` row,
//! and a `quantitySource` on a non-usage row whose kind is not `per_unit` — and
//! neither is decided by a frozen operand or resolved by retraction; a later
//! `model_kind` resolves both. `POST /prices` refuses them at the door, so a header
//! saying the first *"waits for publish"* sends a reader debugging a `400` on a
//! flat-plus-bands body looking for a different cause. What is publish-stage is the
//! fault with an **absent** operand, and a content pair whose resolving call
//! legitimately arrives later — the second argued rule by rule rather than by a
//! criterion, with [`Stage`](crate::domain::validation::Stage) carrying the current
//! state of it.
//!
//! `EVAL_POLICY_MISPLACED` still appears on **both** sides of this line: it is the
//! code for `billingGranularity` on a recurring key, refused here, and for
//! `manual_quantity` sent without `quantitySource = manual`, which is not — two
//! content fields, and the rule reports it at publish. The refusal is the
//! same enumerated `ValidationFailed` envelope publish uses, not a single message,
//! and the publish pre-check still runs the whole set, this subset included.
//!
//! What this plane refuses **besides** that is what the store itself decides — a duplicate
//! canonical scope key (`DUPLICATE_SCOPE_KEY`, on the draft plane too since
//! D-148), a stale entity tag (`STALE_VERSION`), a horizon off its eligibility
//! class (`GRANDFATHER_UNTIL_FORBIDDEN`), an instant finer than the millisecond
//! quantum (`TIMESTAMP_PRECISION_EXCEEDED`), a value past its column
//! (`InvalidArgument`), and an edit of a frozen row (`LIFECYCLE_FORBIDDEN`) —
//! plus the two idempotency refusals the create's gate can raise. Every one of
//! those already exists; nothing here is minted. The two Slice-10 primitives
//! this surface refuses (below) mint nothing either — an unsupported field is a
//! malformed request under the Foundation validation envelope.
//!
//! # The `{planId}` segment is checked, not decorative
//!
//! `GET`, `PATCH` and `DELETE` name a plan **and** a price, while the repository
//! keys on `price_id` alone. So those verbs verify the row actually belongs to
//! the named plan and answer 404 otherwise. A path that names a parent it does
//! not check is a path that lets a caller read or mutate a row through the
//! wrong plan's URL — and it makes the authz `resource_id` argument a fiction,
//! since the gate is asked about a resource the handler then does not confirm.
//!
//! # What Slice 10 has not landed is refused here, not validated
//!
//! `tierQualificationWindow` (D-40, D-60) and the **`carry`** half of
//! `includedAllowance` (D-45, D-130) are members of [`PriceContentView`] and are
//! refused with [`DomainError::InvalidRequest`] the moment a request carries
//! one.
//!
//! **The allowance's `none` half is no longer among them.** D-177 clause (3)
//! made this refusal removable only in the change that lands the rules *and* the
//! compile, and that is the change: `inst-ac-gate`'s six refusals are registered
//! row-local rules ([`crate::domain::rules::allowance`]) that this very write
//! runs, and `inst-ac-band` / `inst-ac-marker` are
//! [`crate::domain::allowance`], materialized into the read model by the
//! projector. A `{N, none}` declaration is therefore judged when it arrives and
//! honoured when it publishes, which is the whole of what the refusal was
//! holding the line for.
//!
//! What is still refused, and why it is a store rather than a rule:
//!
//! - **`tierQualificationWindow`, at any value** — none of `inst-tt-forbidden` /
//!   `inst-tt-window-pair` / `inst-tt-zero-band` / `inst-tt-fixture` exists, nor
//!   does the rate lock (`inst-tt-lock`). Storing a value this gear can neither
//!   judge nor honour is precisely the state `inst-tt-forbidden` names when it
//!   says *an accepted-but-ignored value would mask authoring errors*.
//! - **`includedAllowance` with `rolloverPolicy = carry`** — `inst-ac-carry`
//!   materializes a `promotional` grant into `pricing_plan_grant` (D-52), a
//!   table this gear does not have, so the carried allowance would freeze into
//!   an immutable version and never be issued.
//!
//! The projector and the evaluation-policy roster carry both fields, so a stored
//! value that reached publish would freeze. *(The publish route is mounted and the
//! freeze does not follow, because `publish::rules::NoUnjudgedPrimitive` refuses what
//! is still unbuilt inside the publish set — see D-298. The guard on this surface
//! still earns its place, because a row stored here that publish then refuses is a
//! row nobody can publish.)*
//!
//! Building the refusals **without** the compile would have been worse, not
//! better: a `graduated` row carrying `{100, none}` would have passed all six
//! `inst-ac-gate` rules, published, and then been billed from unit one — an
//! allowance accepted, *validated*, and silently ignored. That is the shape
//! D-149 clause (3), D-161 clause (1), D-167 clause (3) and D-168 clause (1)
//! each refuse one artifact over, and it is why the six and the compile landed
//! in one change.
//!
//! The members **stay** on the view. D-174 clause (1) puts a member the gear
//! does not model outside the idempotency digest and therefore inside the replay
//! set, so deleting them would convert an accepted field into a *silently
//! ignored* one. The response half stays too: the domain model, the storage
//! round trip and the D-129 supersession guard all read both fields, and a read
//! that dropped them would lose a field that guard compares. No code is minted
//! — a field whose slice has not landed is a malformed request under the
//! Foundation validation envelope, the class D-141 and D-171 give an absent
//! `If-Match`.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Extension, Path, Query};
use axum::http::header::{ETAG, LOCATION};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, http::HeaderMap, http::StatusCode};
use bss_fixtures::ModelKind;
use bss_pricing_sdk::odata::PlanPriceFilterField;

use toolkit::api::canonical_prelude::CanonicalError;
use toolkit::api::odata::OData;
use toolkit::api::operation_builder::OperationBuilderODataExt;
use toolkit::api::{OpenApiRegistry, operation_builder::OperationBuilder};
use toolkit_db::secure::{AccessScope, DbTx};
use toolkit_odata::Page;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::auth_context::{audit_stamp, require_authenticated};
use crate::api::rest::correlation::{CorrelationId, require_correlation};
use crate::api::rest::error::authz_error_to_canonical;
use crate::api::rest::odata_list::{map_odata_page_err, reject_non_odata_list_params};
use crate::api::rest::preconditions;
use crate::api::rest::state::AuthoringState;
use crate::domain::contracts::{AnchorDay, BillingAnchorPolicy, ProrationBasis, ProrationContract};
use crate::domain::error::DomainError;
use crate::domain::instant::rfc3339;
use crate::domain::lifecycle::LifecycleState;
use crate::domain::money::{CurrencyCode, MinorAmount, RateMinor};
use crate::domain::price_record::{PriceContent, PriceRecord};
use crate::domain::price_row::{
    AggregationFunction, AggregationGranularity, BandTop, BillingGranularity, IncludedAllowance,
    MinQtyUsageFallback, PriceRow, QuantitySource, ReservationFlavor, RolloverPolicy,
    TierAggregationWindow, TierBand, TierQualificationWindow, model_kind_wire,
};
use crate::domain::scope_key::{
    ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use crate::infra::idempotent::{self, Guarded, GuardedRequest, TxFuture};
use crate::infra::storage::repo::{NewPriceDraft, price_repo};
use crate::infra::storage::repo_failure;
use time::OffsetDateTime;

/// `OpenAPI` tag applied to every price operation (DE0205).
const TAG: &str = "BSS Pricing Price Rows";

/// The idempotency-gate operation name for the guarded create. Its own, so a
/// client key reused across the two guarded creates does not collide.
const CREATE_PRICE_OPERATION: &str = "bss_pricing.create_price";

/// A plan's price rows.
pub const PLAN_PRICES: &str = "/bss-pricing/v1/plans/{planId}/prices";
/// One price row under its plan.
pub const PLAN_PRICE: &str = "/bss-pricing/v1/plans/{planId}/prices/{priceId}";

/// The lifecycle states the **authoring** list answers with.
///
/// `draft` and `published` — the two an author works with, and the states a
/// `PATCH`, a `DELETE` or a supersession can be aimed at. `superseded` is
/// excluded because it is history that is no longer current on its key, and
/// `abandoned` because it is not a price-row state at all
/// (`03-price-structure.md` §4 has three states). The vocabulary comes from
/// [`LifecycleState`] rather than from string literals, exactly as
/// `infra::publish`'s `CANDIDATE_ROW_STATES` takes it, for the same reason: a
/// literal list is one a state added later goes missing from silently.
///
/// A caller that needs another set is asking for a filter nothing has designed;
/// it is reported rather than invented here.
const AUTHORING_STATES: &[LifecycleState] = &[LifecycleState::Draft, LifecycleState::Published];

// ---------------------------------------------------------------------------
// Views and requests.
// ---------------------------------------------------------------------------

/// The six axes a caller authors. The stored key is ten: `planId` is the path
/// segment, `priceOverlay` is always `base` on this plane, and the usage pair is
/// derived from the row's content (D-196 clause (3)).
///
/// `price_overlay` is **not** a member: every row this gear authors carries
/// `base`, and partner / orgTier / brand overlays are separate overlay rows
/// rather than a value of this axis. Offering it would offer a choice the
/// authoring plane does not have.
///
/// `plan_id` is not a member either — it is the `{planId}` path segment, so a
/// body cannot name a plan the URL does not.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct ScopeKeyRequest {
    /// ISO 4217 currency.
    pub currency: String,
    /// The pricing region.
    pub region: String,
    /// The phase this row prices; the plan's terminal phase for a
    /// phase-invariant row (D-19).
    pub phase: Uuid,
    /// `all_subscriptions` | `new_subscriptions_only` | `existing_grandfathered`.
    pub price_eligibility: String,
    /// `recurring` | `usage` | `one_time` | `one_time_setup`.
    pub charge_kind: String,
    /// The grandfathering generation's cutover instant, or `null` for a row
    /// that retains nobody.
    #[serde(default, with = "rfc3339::option")]
    pub cohort: Option<OffsetDateTime>,
}

/// The ten axes as the store holds them.
///
/// **Two more members than [`ScopeKeyRequest`], and the asymmetry is D-196's.**
/// The usage line is authored on the *content* view — a request naming it on the
/// key would be a second place to state one fact — so `ScopeKeyRequest` has no
/// `meter` and no `dimension_key`, while the key a row is **filed under** carries
/// both. This view is the store's rendering, not an echo of the request, and
/// rendering eight answers two rows on two meters of one market with
/// byte-identical `scope_key` objects — a reviewer reading the approval surface
/// sees one key twice and no way to tell which line they are approving.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ScopeKeyView {
    /// Axis 1.
    pub plan_id: Uuid,
    /// Axis 2.
    pub currency: String,
    /// Axis 3.
    pub region: String,
    /// Axis 4 — always `base` on an authored row.
    pub price_overlay: String,
    /// Axis 5.
    pub phase: Uuid,
    /// Axis 6.
    pub price_eligibility: String,
    /// Axis 7.
    pub charge_kind: String,
    /// Axis 8, `null` when the row retains nobody.
    #[serde(default, with = "rfc3339::option")]
    pub cohort: Option<OffsetDateTime>,
    /// Axis 9 — the metering unit, `null` on a row that is not metered (D-196).
    pub meter: Option<String>,
    /// Axis 10 — the dimension discriminator on the line, `null` for the
    /// undimensioned one (D-196).
    ///
    /// `null` rather than the store's `''` sentinel, for [`Self::cohort`]'s
    /// reason: an empty string on the wire is a value a caller can read as one,
    /// and "no dimension" is an absence. The sentinel is the column's business.
    pub dimension_key: Option<String>,
}

impl From<&ScopeKey> for ScopeKeyView {
    fn from(key: &ScopeKey) -> Self {
        Self {
            plan_id: key.plan_id().get(),
            currency: key.currency().as_str().to_owned(),
            region: key.region().as_str().to_owned(),
            price_overlay: key.price_overlay().as_str().to_owned(),
            phase: key.phase().get(),
            price_eligibility: key.price_eligibility().as_str().to_owned(),
            charge_kind: key.charge_kind().as_str().to_owned(),
            cohort: key.cohort().generation(),
            meter: key.meter().map(|meter| meter.as_str().to_owned()),
            dimension_key: (!key.dimension_key().is_none())
                .then(|| key.dimension_key().as_str().to_owned()),
        }
    }
}

/// One tier band.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct TierBandView {
    /// Inclusive lower bound.
    pub from_qty: u64,
    /// Exclusive upper bound, or `null` for the open top band (D-17: the top
    /// band is always open, and capping belongs to quotas).
    pub to_qty: Option<u64>,
    /// The band's **rate**, in 10^-9 ISO-4217 minor units (D-311).
    ///
    /// **Renamed rather than reinterpreted.** It was `unit_price_minor` carrying
    /// whole minor units; a rate is a multiplier and needs finer resolution than
    /// an invoice amount, so the same commercial price is now a different
    /// integer. Keeping the old name would have multiplied every band rate by a
    /// billion in every consumer that did not know — silently, and this is the
    /// field a mis-read prices an entire ladder at nothing. An unknown name
    /// reads as absent, which is a visible blank rather than a wrong number.
    pub unit_price_nano_minor: i64,
}

impl From<&TierBand> for TierBandView {
    fn from(band: &TierBand) -> Self {
        Self {
            from_qty: band.from_qty,
            to_qty: band.to_qty.closed_at(),
            unit_price_nano_minor: band.unit_price_rate.nano_minor(),
        }
    }
}

/// The included allowance a usage row grants (D-45).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct IncludedAllowanceView {
    /// How much is included.
    pub quantity: u64,
    /// `none` | `carry`.
    pub rollover_policy: String,
}

/// Everything about a row an open draft may still change.
///
/// It is a whole-content submission and not a patch, for the reason
/// [`PriceContent`]'s own doc gives: a price row's fields are not independent of
/// each other — moving `model_kind` from `graduated` to `flat` has to drop the
/// band set and set `amount_minor` in the *same* write, because every
/// intermediate state is one no Slice-3 rule can pass.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct PriceContentView {
    /// `flat` | `per_unit` | `graduated` | `volume` | `package`. Authored, never
    /// inferred — there is no implicit default at rating time.
    pub model_kind: Option<String>,
    /// The row's own amount, on the kinds that carry one.
    pub amount_minor: Option<i64>,
    /// The `per_unit` **rate**, in 10^-9 ISO-4217 minor units (D-311).
    ///
    /// Its own member rather than a second meaning for `amount_minor`, which
    /// carried the `flat` sum *and* the `per_unit` price — an invoice amount and
    /// a multiplier under one name. A `per_unit` row sets this and leaves
    /// `amount_minor` absent.
    pub unit_rate_nano_minor: Option<i64>,
    /// The band set, ordered by lower bound on read.
    pub bands: Option<Vec<TierBandView>>,
    /// The package's block size.
    pub package_size: Option<u64>,
    /// The package's block price.
    pub package_price_minor: Option<i64>,
    /// Where a non-usage `per_unit` row's quantity comes from.
    pub quantity_source: Option<String>,
    /// The manual quantity, on a `manual` source.
    pub manual_quantity: Option<u64>,
    /// The meter a usage row prices.
    pub meter: Option<String>,
    /// The priced dimension within the meter; absent on a whole-meter line, as
    /// it is on the scope key this row's view carries beside it.
    ///
    /// Emitted as `null` for that absence and **accepted** as `null`, as the
    /// empty string, or omitted — all three decode to the same undimensioned
    /// line, so a client echoing back a body this surface issued round-trips
    /// whichever spelling it holds.
    pub dimension_key: Option<String>,
    /// The billable-unit quantization.
    pub billing_granularity: Option<String>,
    /// The tier counter's reset window.
    pub tier_aggregation_window: Option<String>,
    /// The D-40 tier-qualification window.
    pub tier_qualification_window: Option<String>,
    /// How the in-window quantity is derived (D-44).
    pub aggregation_function: Option<String>,
    /// The granule a non-`sum` window is cut into (D-44).
    pub aggregation_granularity: Option<String>,
    /// The `maxHold` bound on a non-`sum` row.
    pub max_hold_granules: Option<u64>,
    /// The plan-scoped included allowance (D-45).
    pub included_allowance: Option<IncludedAllowanceView>,
    /// The reserved rate, in the row's currency (`inst-rv-attrs`).
    ///
    /// This field and the five below it are Slice 10's authoring surface, and
    /// they are on **this** view rather than one of their own because
    /// `inst-ad-author` says so: primitives "author through the Slice 2/3
    /// plan/price PATCH surfaces", row-attached ones on the price row. S10 §5
    /// declares no endpoint of its own for the same reason.
    pub reserved_rate_nano_minor: Option<i64>,
    /// What the reservation reserves: `consumption | capacity` (`inst-rv-attrs`).
    pub reservation_flavor: Option<String>,
    /// The order-time purchase floor (`inst-ft-typed`).
    pub min_qty_purchase: Option<u64>,
    /// The eligibility usage floor (`inst-ft-typed`).
    pub min_qty_usage: Option<u64>,
    /// What happens beneath the usage floor; launch: `exception`
    /// (`inst-ft-fallback`).
    pub min_qty_usage_fallback: Option<String>,
    /// The external discount instrument (`inst-dr-referential`).
    pub discount_ref: Option<String>,
    /// Whether the authored amounts are tax-inclusive. Absent is `false`.
    pub tax_inclusive: Option<bool>,
    /// The row's tax category (D-110) — the **source of truth**, and the only
    /// place one lives. Absent states none, and D-154 then resolves the region
    /// taxonomy's default at publish; it is not the same as "no category".
    pub tax_category_ref: Option<String>,
    /// `advance` | `arrears` — Slice-6-owned, so a free string here.
    pub billing_timing: Option<String>,
    /// Where the subscription's cycle boundary falls: `calendar_month` |
    /// `subscription_start` | `fixed_day` (K2). Required on a recurring row
    /// together with the two below.
    pub billing_anchor_policy: Option<String>,
    /// The day a `fixed_day` policy anchors on, 1–31. Refused beside any other
    /// policy, and required beside `fixed_day`.
    pub anchor_day: Option<u8>,
    /// The canonical K1 basis: `calendar_days_actual` | `calendar_days_30` |
    /// `by_second` | `whole_unit` | `none`.
    pub proration_basis: Option<String>,
    /// Whether a downgrade off this row is credit-eligible.
    pub credit_on_downgrade: Option<bool>,
    /// The named rounding policy this row resolves against.
    pub rounding_policy_ref: Option<String>,
    /// The grandfathering horizon. Only an `existing_grandfathered` row may
    /// carry one.
    #[serde(default, with = "rfc3339::option")]
    pub grandfather_until: Option<OffsetDateTime>,
    /// The predecessor this row supersedes on its key.
    pub supersedes_price_id: Option<Uuid>,
}

impl From<&PriceRecord> for PriceContentView {
    fn from(record: &PriceRecord) -> Self {
        let row = &record.row;
        Self {
            model_kind: row.model_kind.map(model_kind_wire).map(str::to_owned),
            amount_minor: row.amount_minor.map(MinorAmount::get),
            unit_rate_nano_minor: row.unit_rate.map(RateMinor::nano_minor),
            bands: Some(row.bands.iter().map(TierBandView::from).collect()),
            package_size: row.package_size,
            package_price_minor: row.package_price_minor.map(MinorAmount::get),
            quantity_source: row.quantity_source.map(|q| q.as_str().to_owned()),
            manual_quantity: row.manual_quantity,
            meter: row.meter.clone(),
            // `null` for an absent line, which is what `ScopeKeyView` renders
            // for the same fact -- and a `PriceRowView` carries both, this
            // member on its `content` and that one on its `scope_key`.
            // `Some("")` here costs a consumer joining the two: the join says
            // the row is dimensioned differently from its own key, on every
            // undimensioned line.
            dimension_key: (!row.dimension_key.is_empty()).then(|| row.dimension_key.clone()),
            billing_granularity: row.billing_granularity.map(|g| g.as_str().to_owned()),
            tier_aggregation_window: row.tier_aggregation_window.map(|w| w.as_str().to_owned()),
            tier_qualification_window: row.tier_qualification_window.map(|w| w.as_str().to_owned()),
            aggregation_function: row.aggregation_function.map(|f| f.as_str().to_owned()),
            aggregation_granularity: row.aggregation_granularity.map(|g| g.as_str().to_owned()),
            max_hold_granules: row.max_hold_granules,
            included_allowance: row.included_allowance.as_ref().map(|allowance| {
                IncludedAllowanceView {
                    quantity: allowance.quantity,
                    rollover_policy: allowance.rollover_policy.as_str().to_owned(),
                }
            }),
            reserved_rate_nano_minor: row.reserved_rate.map(RateMinor::nano_minor),
            reservation_flavor: row.reservation_flavor.map(|f| f.as_str().to_owned()),
            min_qty_purchase: row.min_qty_purchase,
            min_qty_usage: row.min_qty_usage,
            min_qty_usage_fallback: row.min_qty_usage_fallback.map(|f| f.as_str().to_owned()),
            discount_ref: row.discount_ref.clone(),
            tax_inclusive: Some(record.tax_inclusive),
            tax_category_ref: record.tax_category_ref.clone(),
            billing_timing: record.billing_timing.clone(),
            billing_anchor_policy: record
                .proration_contract
                .map(|c| c.billing_anchor_policy.as_str().to_owned()),
            anchor_day: record
                .proration_contract
                .and_then(|c| c.billing_anchor_policy.anchor_day())
                .map(AnchorDay::get),
            proration_basis: record
                .proration_contract
                .map(|c| c.proration_basis.as_str().to_owned()),
            credit_on_downgrade: record.proration_contract.map(|c| c.credit_on_downgrade),
            rounding_policy_ref: record.rounding_policy_ref.clone(),
            grandfather_until: record.grandfather_until,
            supersedes_price_id: record.supersedes_price_id,
        }
    }
}

/// One price row, whole.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PriceRowView {
    /// The row's identity, minted by the surface at creation.
    pub price_id: Uuid,
    /// The ten axes it is filed under.
    pub scope_key: ScopeKeyView,
    /// What the row says.
    pub content: PriceContentView,
    /// `draft` | `published` | `superseded`.
    pub lifecycle_state: String,
    /// Pseudonymous principal id of the authoring actor.
    pub created_by: Uuid,
    /// When the row was authored, UTC.
    #[serde(with = "rfc3339")]
    pub created_at_utc: OffsetDateTime,
    /// The row's own optimistic-concurrency version — never the plan's (D-141),
    /// and the same number the `ETag` quotes.
    pub row_version: u64,
}

impl From<&PriceRecord> for PriceRowView {
    fn from(record: &PriceRecord) -> Self {
        Self {
            price_id: record.price_id,
            scope_key: ScopeKeyView::from(&record.scope_key),
            content: PriceContentView::from(record),
            lifecycle_state: record.lifecycle_state.as_str().to_owned(),
            created_by: record.created_by,
            created_at_utc: record.created_at_utc,
            row_version: record.row_version.get(),
        }
    }
}

/// A create: the key the row is filed under, and what it says.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct CreatePriceRequest {
    /// The six axes a caller authors; the seventh is the `{planId}` segment.
    pub scope_key: ScopeKeyRequest,
    /// The row's whole content.
    pub content: PriceContentView,
}

/// An edit: the whole content, and optionally the key it must still be on.
///
/// **The scope key is immutable.** `PriceRepo::update_draft` cannot move it, so
/// a body naming a different one is refused rather than silently ignored: a key
/// decides which duplicate a row is, which supersession chain it joins and which
/// window covers it, and moving it would need the create-time duplicate check
/// re-run against a different key — which is what deleting the draft and
/// authoring another one already is.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct PatchPriceRequest {
    /// The key the caller believes the row is on. Optional; when present it must
    /// equal the stored one.
    pub scope_key: Option<ScopeKeyRequest>,
    /// The row's whole content, replacing what is there.
    pub content: PriceContentView,
}

/// Build the Axum router for the price surface and register its operations.
///
/// No route declares a 422: §3.3's status-rendering rule makes every
/// architectural 422 in the design set reach the wire as a 400 carrying its
/// code, so a 422 here would document a response no path can emit.
#[allow(
    clippy::too_many_lines,
    reason = "one builder chain per operation; flat is clearer than helpers that hide which route declares which response"
)]
pub fn router(state: Arc<AuthoringState>, openapi: &dyn OpenApiRegistry) -> Router {
    let mut router = Router::new();

    router = OperationBuilder::post("/bss-pricing/v1/plans/{planId}/prices")
        .operation_id("bss_pricing.create_price")
        .summary("Create a draft price row on a canonical scope key")
        .description(
            "Creates a `draft` price row and its tier bands in one transaction, and answers \
             `201` with a `Location` header naming the row and an `ETag` carrying its own row \
             version. The `price_id` is minted by the server. An `Idempotency-Key` header is \
             required: the gate runs in the same transaction as the insert, so a retry carrying \
             the same key and body is answered the recorded response - the original price id \
             included - and a retry with a different body is refused \
             `IDEMPOTENCY_PAYLOAD_MISMATCH`. A key already held by a `draft` or `published` row \
             is refused `DUPLICATE_SCOPE_KEY` (on the draft plane too, since D-148). The row's \
             `charge_kind` is the key's, not the body's, so the response echoes what was stored \
             rather than what was sent. Slice-3 shape rules run at publish, not here - with \
             one exception: content that contradicts the frozen scope key (a `model_kind` the \
             key's `charge_kind` forbids, `billingGranularity` on a non-usage key) is refused \
             `400` here, carrying the same enumerated violation report the publish pre-check \
             answers rather than a single message (D-312). A row missing its kind, its amount \
             or its bands still saves - that is the multi-call authoring this door protects.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("planId", "The plan the row prices.")
        .param(crate::api::rest::plans::idempotency_key_param())
        .json_request::<CreatePriceRequest>(openapi, "The row's scope key and content.")
        .handler(create_price)
        .json_response_with_schema::<PriceRowView>(
            openapi,
            StatusCode::CREATED,
            "The newly created draft row.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::get(PLAN_PRICE)
        .operation_id("bss_pricing.get_price")
        .summary("Read one price row by id")
        .description(
            "Returns the named row when it belongs to `{planId}`, with its whole content. This \
             is the authoring read (`plan` x `read`); the create's `Location` is this path. The \
             `ETag` header carries the row's own version (D-141) and is what `PATCH` and \
             `DELETE` take as `If-Match`. A row belonging to a different plan than the \
             `{planId}` segment answers `404`, exactly as an absent row does.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("planId", "The plan the row belongs to.")
        .path_param("priceId", "The row to read.")
        .param(crate::api::rest::plans::if_none_match_param())
        .handler(get_price)
        .json_response_with_schema::<PriceRowView>(openapi, StatusCode::OK, "The named row.")
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

    router = OperationBuilder::patch("/bss-pricing/v1/plans/{planId}/prices/{priceId}")
        .operation_id("bss_pricing.patch_price")
        .summary("Replace a draft row's content")
        .description(
            "Replaces the whole content of a `draft` row, band set included, under the \
             `If-Match` precondition (D-141). It is a whole-content submission rather than a \
             field patch because a row's fields are not independent: moving `model_kind` from \
             `graduated` to `flat` has to drop the bands and set `amount_minor` in the same \
             write. The canonical scope key is **immutable** - a body naming a different one is \
             `400`, never a silent no-op. A row belonging to a different plan than the `{planId}` \
             segment answers `404`. Because the key is immutable, content that contradicts it is \
             refused `400` here as it is on the create, carrying the publish pre-check's own \
             enumerated violation report (D-312) - so an edit of a row stored before that check \
             existed has to fix the contradiction before any other change to it will save.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("planId", "The plan the row belongs to.")
        .path_param("priceId", "The row to replace.")
        .param(crate::api::rest::plans::if_match_param(
            "On a price route the tag is the row's **own** version and nothing else (D-141: \
             never derived from the plan's), because the path addresses one row by id.",
        ))
        .json_request::<PatchPriceRequest>(openapi, "The row's whole new content.")
        .handler(patch_price)
        .json_response_with_schema::<PriceRowView>(
            openapi,
            StatusCode::OK,
            "The row as it stands after the edit.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/bss-pricing/v1/plans/{planId}/prices/{priceId}")
        .operation_id("bss_pricing.delete_price")
        .summary("Delete a never-published draft row")
        .description(
            "Deletes a `draft` row and its bands, under the `If-Match` precondition. The \
             precondition is D-141's whole point: this verb's idempotency cell used to be \
             empty, so a draft row could be destroyed under an unknown version - and what a \
             blind delete destroys is a concurrent editor's uncommitted work, not the row. A \
             published row is refused, never deleted (`inst-ps-nodelete`). A row belonging to a \
             different plan than the `{planId}` segment answers `404`. \
             \
             A row a **price window** stands on is refused with `PRICE_WINDOW_SCHEDULED`, naming \
             the window. That refusal is permanent rather than a retry: a window is never \
             deleted, only cancelled, so the row stays bound. Cancel the window and the draft \
             stands unpublished, pricing nothing.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("planId", "The plan the row belongs to.")
        .path_param("priceId", "The draft row to delete.")
        .param(crate::api::rest::plans::if_match_param(
            "On a price route the tag is the row's **own** version and nothing else (D-141).",
        ))
        .handler(delete_price)
        .no_content_response(
            StatusCode::NO_CONTENT,
            "The draft row and its bands are gone.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/bss-pricing/v1/plans/{planId}/prices")
        .operation_id("bss_pricing.list_plan_prices")
        .summary("List a plan's authoring price rows (cursor-paginated)")
        .description(
            "One page of the plan's `draft` and `published` rows, in `price_id` order, with an \
             opaque `cursor` and a `limit` whose server default is 100 and whose hard cap is \
             1,000 (D-125). Omitting `$filter` keeps that authoring default. \
             `$filter=lifecycle_state eq 'superseded'` is how superseded rows appear here; they \
             also remain on `/history`. `next_cursor` is returned on every page until the \
             result is exhausted. `prev_cursor` is always `null`: D-125 specifies a forward \
             walk only.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("planId", "The plan whose rows are listed.")
        .query_param_typed(
            "limit",
            false,
            "Rows per page (default 100, hard cap 1,000)",
            "integer",
        )
        .query_param("cursor", false, "Opaque base64url pagination cursor")
        .handler(list_plan_prices)
        .with_odata_filter::<PlanPriceFilterField>()
        .with_odata_orderby::<PlanPriceFilterField>()
        .json_response_with_schema::<Page<PriceRowView>>(
            openapi,
            StatusCode::OK,
            "One page of the plan's authoring price rows.",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
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

// ---------------------------------------------------------------------------
// Handlers.
// ---------------------------------------------------------------------------

/// `POST /plans/{planId}/prices`.
async fn create_price(
    Extension(state): Extension<Arc<AuthoringState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    extension_correlation: Option<Extension<CorrelationId>>,
    Path(plan_id): Path<Uuid>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let correlation = require_correlation(extension_correlation)?;
    let tenant = ctx.subject_tenant_id();
    let plan_id = PlanId::new(plan_id);
    let scope = write_scope(&enforcer, &ctx, plan_id.get(), tenant).await?;

    let body: CreatePriceRequest = preconditions::parse_body(&body)?;
    let client_key = preconditions::idempotency_key(&headers)?;
    let request_hash = preconditions::request_digest(&body)?;
    let key = scope_key_of(plan_id, &body.scope_key)?;
    let content = content_of(&body.content)?;
    // D-312. Without it an author is answered 201 and learns at publish, possibly
    // from a different person on a different day.
    //
    // Ahead of the idempotency gate because every operand is in the request: the
    // same body answers the same way however many times it arrives, so a replay
    // reaching it reaches the same verdict the first call did. The two guards
    // that read the *world* cannot say that, and sit inside the guarded body.
    require_no_key_contradiction(&key, &content)?;
    let now = OffsetDateTime::now_utc();

    let guard = GuardedRequest {
        operation: CREATE_PRICE_OPERATION,
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
        move |txn: &DbTx<'_>| -> TxFuture<'_, PriceRecord> {
            Box::pin(async move {
                // **Inside the guarded body, and on this transaction's runner.**
                // Both read world state, so ahead of the gate their operands are
                // re-read on a retry and judged against a world that has moved
                // since the call they are guarding: a region retired between the
                // create and its retry answered `REGION_UNKNOWN` for a row already
                // filed, telling the caller their write never happened while the
                // recorded 201 sat unread in the dedup cell. That is the inversion
                // the mandatory `Idempotency-Key` exists to prevent, and
                // `customer_groups::create_membership` states it at length. A
                // replay does not reach this closure at all.
                //
                // Only the region half has a reachable inversion — no DELETE is
                // permitted on `pricing_plan`, so a plan that existed still does.
                // The plan read moves anyway: it is the same class, and left
                // outside it is a round trip every replay pays for an answer it
                // will not use.
                //
                // The plan first: a row whose plan does not exist has no subject,
                // so asking whether its region is declared is asking about nothing.
                //
                // Still two guards in series and still not redundant: the
                // publish-time `RegionsDeclared` is what holds against a region
                // retired between this save and the publish, which no save-time
                // read can see.
                require_existing_plan(txn, &scope_for_body, tenant, plan_id).await?;
                require_declared_region(txn, &scope_for_body, tenant, &key).await?;
                // Minted inside the guarded body for the reason the plan create
                // states: a replay must answer the FIRST caller's id.
                let draft = NewPriceDraft {
                    price_id: Uuid::now_v7(),
                    scope_key: key,
                    content,
                    created_by: actor,
                    created_at_utc: now,
                    correlation_id: correlation,
                };
                // `guarded`'s mutation speaks `DomainError`; the ladder is the
                // same one it used to apply here. See `plans.rs`'s note.
                Box::pin(price_repo::create_draft_on(
                    txn,
                    &scope_for_body,
                    tenant,
                    draft,
                ))
                .await
                .map_err(|e| repo_failure(&e))
            })
        },
        |record: &PriceRecord| {
            serde_json::to_value(PriceRowView::from(record)).map_err(|e| {
                DomainError::Internal(format!("cannot render the created price row: {e}"))
            })
        },
    )
    .await
    .map_err(CanonicalError::from)?;

    Ok(match outcome {
        Guarded::Performed(record) => created(plan_id, &record),
        Guarded::Replayed { status, body } => {
            replayed(CREATE_PRICE_OPERATION, plan_id, status, &body)?
        }
    })
}

/// `GET /plans/{planId}/prices/{priceId}`.
///
/// The gate is `plan x read` with `resource_id = planId`, like
/// [`list_plan_prices`]: the authz catalog puts `plan × read` on the plan, and
/// [`row_of_plan`] binds the named row to that plan. A matching
/// `If-None-Match` is 304 — a read that emits a validator honours the
/// conditional.
async fn get_price(
    Extension(state): Extension<Arc<AuthoringState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Path((plan_id, price_id)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
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

    let record = row_of_plan(&state, &scope, ctx.subject_tenant_id(), plan_id, price_id).await?;
    let tag = preconditions::etag(record.row_version);
    if preconditions::if_none_match(&headers, &tag) {
        return Ok(preconditions::not_modified(&tag));
    }
    Ok(answer(&record).into_response())
}

/// `PATCH /plans/{planId}/prices/{priceId}`.
async fn patch_price(
    Extension(state): Extension<Arc<AuthoringState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    extension_correlation: Option<Extension<CorrelationId>>,
    Path((plan_id, price_id)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<([(axum::http::HeaderName, String); 1], Json<PriceRowView>), CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let correlation = require_correlation(extension_correlation)?;
    let tenant = ctx.subject_tenant_id();
    let plan_id = PlanId::new(plan_id);
    // **The plan id, not the price row's** — the object the authz catalog's
    // endpoint map puts `plan × write` on. `windows.rs` states the rule at length
    // and restructures three handlers rather than pass a window id the PDP carries
    // no label for. Passing `price_id` here has a role definition of the form
    // "allow `plan × write` where `resource_id in {planA}`" — which
    // `SUPPORTED_PROPERTIES` advertises precisely so one can be written —
    // evaluated against a price row's id and denied. The direction is
    // availability rather than escalation, and `row_of_plan` below already binds
    // the row to the plan.
    let scope = write_scope(&enforcer, &ctx, plan_id.get(), tenant).await?;

    let body: PatchPriceRequest = preconditions::parse_body(&body)?;
    let expected = preconditions::if_match(&headers)?;
    let stored = row_of_plan(&state, &scope, tenant, plan_id, price_id).await?;
    if let Some(named) = &body.scope_key {
        // **Compared over the axes the wire can express** (D-196 clause 3).
        // `ScopeKeyRequest` has no `meter` member — the usage line is authored on
        // the *content* view and the door derives the ninth and tenth axes from
        // it — so a stored usage row's key always carries a line this body could
        // not have named. Comparing the two raw would refuse every `PATCH` that
        // echoes its own key on a metered row, which is the opposite of what this
        // immutability check is for. The stored line is therefore carried onto
        // the named key before the comparison: the axes the caller *can* state
        // must match, and the ones they cannot are taken from the row.
        let named = scope_key_of(plan_id, named)?.with_usage_line(
            stored.scope_key.meter().cloned(),
            stored.scope_key.dimension_key().clone(),
        )?;
        if named != stored.scope_key {
            return Err(CanonicalError::from(DomainError::InvalidRequest(
                "the canonical scope key is immutable; a row's key decides which duplicate it \
                 is, which supersession chain it joins and which window covers it. Delete this \
                 draft and author another one on the key you want"
                    .to_owned(),
            )));
        }
    }
    let content = content_of(&body.content)?;
    // The stored key, because the key is immutable and the block above has already
    // refused a body that names a different one. `PATCH` carries the check as well
    // as `POST` because an existing row is edited through it, and covering the
    // create alone would leave that door open — it is the door the Studio actually
    // used. These two are not, however, the *only* doors a row enters through:
    // `bulk_imports` is the third, and it carries this same line on its own phase-1
    // report rather than through this handler (D-312, "Three doors, not two"). The
    // decision entry used to assert that import could not land such a row, which was
    // measured false — so do not read this pair as exhaustive.
    require_no_key_contradiction(&stored.scope_key, &content)?;

    let updated = state
        .prices
        .update_draft(
            &scope,
            tenant,
            price_id,
            expected,
            content,
            audit_stamp(&ctx, OffsetDateTime::now_utc(), correlation),
            // An interactive edit belongs to no run, so the bulk lock excludes it
            // whoever holds the row (`inst-bk-lock`).
            /* on_behalf_of */
            None,
        )
        .await
        .map_err(|e| CanonicalError::from(repo_failure(&e)))?;

    Ok(answer(&updated))
}

/// `DELETE /plans/{planId}/prices/{priceId}`.
async fn delete_price(
    Extension(state): Extension<Arc<AuthoringState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    extension_correlation: Option<Extension<CorrelationId>>,
    Path((plan_id, price_id)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
) -> Result<StatusCode, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    let correlation = require_correlation(extension_correlation)?;
    let tenant = ctx.subject_tenant_id();
    let plan_id = PlanId::new(plan_id);
    // **The plan id, not the price row's** — the object the authz catalog's
    // endpoint map puts `plan × write` on. `windows.rs` states the rule at length
    // and restructures three handlers rather than pass a window id the PDP carries
    // no label for. Passing `price_id` here has a role definition of the form
    // "allow `plan × write` where `resource_id in {planA}`" — which
    // `SUPPORTED_PROPERTIES` advertises precisely so one can be written —
    // evaluated against a price row's id and denied. The direction is
    // availability rather than escalation, and `row_of_plan` below already binds
    // the row to the plan.
    let scope = write_scope(&enforcer, &ctx, plan_id.get(), tenant).await?;

    let expected = preconditions::if_match(&headers)?;
    row_of_plan(&state, &scope, tenant, plan_id, price_id).await?;

    state
        .prices
        .delete_draft(
            &scope,
            tenant,
            price_id,
            expected,
            audit_stamp(&ctx, OffsetDateTime::now_utc(), correlation),
            // An interactive delete belongs to no run, so the bulk lock excludes
            // it whoever holds the row (`inst-bk-lock`) — the sibling `PATCH`'s
            // argument one verb over.
            /* on_behalf_of */
            None,
        )
        .await
        .map_err(|e| CanonicalError::from(repo_failure(&e)))?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /plans/{planId}/prices`.
///
/// The gate is `plan x read` with `owner_tenant_id = None`, so the compiled
/// scope is the SQL filter and the whole walk is tenant-scoped by construction
/// rather than by a predicate this handler remembers to add.
#[allow(clippy::implicit_hasher)]
async fn list_plan_prices(
    Extension(state): Extension<Arc<AuthoringState>>,
    Extension(enforcer): Extension<authz_resolver_sdk::PolicyEnforcer>,
    extension_ctx: Option<Extension<SecurityContext>>,
    Path(plan_id): Path<Uuid>,
    Query(extras): Query<HashMap<String, String>>,
    OData(odata): OData,
) -> Result<Json<Page<PriceRowView>>, CanonicalError> {
    let ctx = require_authenticated(extension_ctx)?;
    reject_non_odata_list_params(&extras)?;
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

    let page = state
        .prices
        .list_for_plan_odata(
            &scope,
            ctx.subject_tenant_id(),
            plan_id,
            &odata,
            AUTHORING_STATES,
        )
        .await
        .map_err(map_odata_page_err)?;
    Ok(Json(Page {
        items: page.items.iter().map(PriceRowView::from).collect(),
        page_info: page.page_info,
    }))
}

// ---------------------------------------------------------------------------
// Shared pieces.
// ---------------------------------------------------------------------------

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

/// The row named by `{priceId}`, **confirmed to belong to `{planId}`**.
///
/// A row under the wrong plan's URL is answered exactly like an absent one: the
/// caller named a resource that does not exist at that address, and telling them
/// the row exists elsewhere would leak which plans hold which rows.
async fn row_of_plan(
    state: &AuthoringState,
    scope: &AccessScope,
    tenant: Uuid,
    plan_id: PlanId,
    price_id: Uuid,
) -> Result<PriceRecord, CanonicalError> {
    let found = state
        .prices
        .find(scope, tenant, price_id)
        .await
        .map_err(|e| CanonicalError::from(repo_failure(&e)))?;
    match found {
        Some(record) if record.scope_key.plan_id() == plan_id => Ok(record),
        _ => Err(CanonicalError::from(DomainError::NotFound {
            subject: "price row".to_owned(),
            id: price_id.to_string(),
        })),
    }
}

/// 200 plus the row's own entity tag.
fn answer(record: &PriceRecord) -> ([(axum::http::HeaderName, String); 1], Json<PriceRowView>) {
    (
        [(ETAG, preconditions::etag(record.row_version))],
        Json(PriceRowView::from(record)),
    )
}

/// The 201 a performed create answers with.
fn created(plan_id: PlanId, record: &PriceRecord) -> Response {
    (
        StatusCode::CREATED,
        [
            (LOCATION, price_location(plan_id, record.price_id)),
            (ETAG, preconditions::etag(record.row_version)),
        ],
        Json(PriceRowView::from(record)),
    )
        .into_response()
}

/// The recorded answer a replay is handed back.
///
/// `Location` is rebuilt from the recorded body's `price_id`, which is a pure
/// function of data that body carries. No `ETag`: the dedup row stores a status
/// and a body and no headers, and the row's version may have moved since — a tag
/// rebuilt from a stale body would be a precondition token that looks valid and
/// is not.
///
/// # Errors
/// [`DomainError::Internal`] when the stored status is not one — see
/// [`super::replayed_status`].
fn replayed(
    operation: &str,
    plan_id: PlanId,
    status: i32,
    body: &serde_json::Value,
) -> Result<Response, DomainError> {
    let status = super::replayed_status(operation, status)?;
    let location = body
        .get("price_id")
        .and_then(serde_json::Value::as_str)
        .and_then(|raw| Uuid::parse_str(raw).ok())
        .map(|price_id| price_location(plan_id, price_id));
    Ok(match location {
        Some(location) => (status, [(LOCATION, location)], Json(body.clone())).into_response(),
        None => (status, Json(body.clone())).into_response(),
    })
}

/// Where a row lives, spelled once.
fn price_location(plan_id: PlanId, price_id: Uuid) -> String {
    format!("/bss-pricing/v1/plans/{plan_id}/prices/{price_id}")
}

// ---------------------------------------------------------------------------
// Wire -> domain.
//
// Every token list below is `price_repo`'s, not a second one: the surface parses
// the same tokens off a request that the repository parses off a column, and two
// tables would be two answers to the same question.
// ---------------------------------------------------------------------------

/// The plan the path names has to exist for this tenant.
///
/// The route files a row under `plan_id`, and every read of a price row goes
/// *through* the plan — so a row whose plan does not exist is a row nothing can
/// ever reach. Without this the surface answered 201 and the operator found out
/// at publish, or never.
///
/// **Not a tenancy check, though it looks like one.** The scope filter has
/// already removed rows this caller may not see, so a foreign plan id and an
/// invented one are indistinguishable here by construction: both resolve to
/// `None`, both answer 404, and neither confirms that some other tenant owns the
/// id. That property is what keeps this guard from becoming an existence oracle,
/// and it is pinned in the e2e suite rather than only asserted here, because it
/// is a claim about two responses being *equal* and no single-request test can
/// see it.
///
/// The generic `NotFound` rather than a minted code: D-146's posture is that a
/// wire code the design set declares must not end up in two spellings, and this
/// refusal is the plain "the thing you named is not here".
///
/// # Errors
/// [`DomainError::NotFound`] when the plan is not this tenant's;
/// [`DomainError::Internal`] on a storage failure.
async fn require_existing_plan(
    runner: &impl toolkit_db::secure::DBRunner,
    scope: &toolkit_db::secure::AccessScope,
    tenant: Uuid,
    plan_id: PlanId,
) -> Result<(), DomainError> {
    let exists =
        crate::infra::storage::repo::plan_repo::max_revision_on(runner, scope, tenant, plan_id)
            .await
            .map_err(|e| repo_failure(&e))?;
    if exists.is_none() {
        return Err(DomainError::NotFound {
            subject: "plan".to_owned(),
            id: plan_id.get().to_string(),
        });
    }
    Ok(())
}

/// Refuse a row whose `region` is not an **active** value of the tenant's region
/// taxonomy (`inst-mc-region`, §2 step 2, C2).
///
/// The `active` set and not merely a declared one, which is
/// `overlay_repo::declares`' predicate one plane over: a value that reached
/// `retired` must not validate a new row against itself, or the retire guard
/// would be a door that closes from only one side.
///
/// Reported with the **same code and the same wording** the publish-time rule
/// uses, through `domain::taxonomy`, so an operator who meets the refusal twice
/// meets one message. A second spelling here would be a second answer to the
/// same question.
///
/// # Errors
/// [`DomainError::RegionUnknown`] carrying `REGION_UNKNOWN` when the region is
/// not declared active; [`DomainError::Internal`] on a storage failure.
async fn require_declared_region(
    runner: &impl toolkit_db::secure::DBRunner,
    scope: &toolkit_db::secure::AccessScope,
    tenant: Uuid,
    key: &ScopeKey,
) -> Result<(), DomainError> {
    let declared =
        crate::infra::storage::repo::taxonomy_repo::active_regions(runner, scope, tenant)
            .await
            .map_err(|e| repo_failure(&e))?;
    let rule = crate::domain::taxonomy::RegionsDeclared { declared };
    if let Some(violation) = rule.violation_for(key.region()) {
        return Err(DomainError::RegionUnknown(violation.detail));
    }
    Ok(())
}

/// D-312 — refuse a row the write can already see is unpublishable.
///
/// The rule set stays where it was: `run_publish_rules` runs all of it, twice, and
/// nothing about §4.2 moves. This runs the **same** pipeline here and keeps only
/// the violations marked [`Stage::Write`](crate::domain::validation::Stage) — the
/// faults whose every operand is in the request with one of them frozen by the
/// scope key. Such a request is complete and knowably unpublishable when it
/// arrives, and the only call that resolves it retracts the field just sent.
///
/// **The charge kind must come from the key, not from the content.** `content_of`
/// builds its `PriceRow` with `ChargeKind::Recurring` as a placeholder — that row
/// is a conversion vehicle for the content columns and its charge kind is a lie by
/// construction. Judging the placeholder would make every `graduated` usage row
/// look like a `graduated` recurring one and refuse it outright, which is this
/// change's most inviting way to be wrong.
///
/// It borrows that rewrite from [`price_repo::authored_content`] rather than
/// open-coding it, because the row this judges has to be **the row the store will
/// file**, not one that merely agrees with it today. `authored_content` is the one
/// spelling of what the write door rewrites — the key's charge kind *and* the band
/// sort — and its own comment says it exists for a second caller to be able to
/// predict them. Judging a differently-rewritten row is how a check starts
/// disagreeing with the write it guards; `prepare_draft` and
/// `refuse_divergent_successor` both record what open-coding this cost before.
///
/// Answers the enumerated report rather than a single message: a client already
/// parsing the publish envelope parses this unchanged, and folding a
/// multi-violation report into one line hides the second fault behind the first.
fn require_no_key_contradiction(
    key: &ScopeKey,
    content: &PriceContent,
) -> Result<(), CanonicalError> {
    let subject = price_repo::authored_content(key, content.clone()).row;
    let report = crate::domain::rules::price_row_rules().run(&subject);
    match report.write_stage_only() {
        None => Ok(()),
        Some(write_stage) => Err(CanonicalError::from(DomainError::ValidationFailed(
            write_stage,
        ))),
    }
}

/// Build the canonical scope key from the path's plan and the body's axes.
pub(crate) fn scope_key_of(
    plan_id: PlanId,
    key: &ScopeKeyRequest,
) -> Result<ScopeKey, DomainError> {
    ScopeKey::new(
        plan_id,
        CurrencyCode::new(&key.currency)?,
        Region::new(&key.region)?,
        PhaseId::new(key.phase),
        wire_token(
            "scope_key.price_eligibility",
            &key.price_eligibility,
            price_repo::PRICE_ELIGIBILITIES,
            PriceEligibility::as_str,
        )?,
        wire_token(
            "scope_key.charge_kind",
            &key.charge_kind,
            price_repo::CHARGE_KINDS,
            ChargeKind::as_str,
        )?,
        key.cohort.map_or(Cohort::None, Cohort::Generation),
    )
}

/// Build the row's content.
///
/// `charge_kind` is filled from the key by the repository, not from here: the
/// axis is the key's and the copy on [`PriceRow`] is a convenience the shape
/// rules read. A placeholder is passed and overwritten, which is why the
/// response echoes what was stored rather than what was sent.
///
/// `pub(crate)` for one second caller, [`crate::api::rest::supersessions`]: a
/// supersession's successor is a price row and its body carries the **same**
/// [`PriceContentView`], so a second conversion of that view would be a second
/// reading of what a price row's content is — including a second copy of
/// `refuse_unlanded_primitives`, which is the one guard a divergence there would
/// silently drop.
pub(crate) fn content_of(view: &PriceContentView) -> Result<PriceContent, DomainError> {
    refuse_unlanded_primitives(view)?;
    let bands = view
        .bands
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(band_of)
        .collect::<Result<Vec<_>, _>>()?;
    let row = PriceRow {
        charge_kind: ChargeKind::Recurring,
        model_kind: view
            .model_kind
            .as_deref()
            .map(|token| {
                wire_token(
                    "content.model_kind",
                    token,
                    &ModelKind::ALL,
                    model_kind_wire,
                )
            })
            .transpose()?,
        amount_minor: amount("content.amount_minor", view.amount_minor)?,
        unit_rate: view
            .unit_rate_nano_minor
            .map(RateMinor::from_nano_minor)
            .transpose()?,
        bands,
        package_size: view.package_size,
        package_price_minor: amount("content.package_price_minor", view.package_price_minor)?,
        quantity_source: optional_token(
            "content.quantity_source",
            view.quantity_source.as_deref(),
            price_repo::QUANTITY_SOURCES,
            QuantitySource::as_str,
        )?,
        manual_quantity: view.manual_quantity,
        meter: view.meter.clone(),
        dimension_key: view.dimension_key.clone().unwrap_or_default(),
        billing_granularity: optional_token(
            "content.billing_granularity",
            view.billing_granularity.as_deref(),
            price_repo::BILLING_GRANULARITIES,
            BillingGranularity::as_str,
        )?,
        tier_aggregation_window: optional_token(
            "content.tier_aggregation_window",
            view.tier_aggregation_window.as_deref(),
            price_repo::TIER_AGGREGATION_WINDOWS,
            TierAggregationWindow::as_str,
        )?,
        tier_qualification_window: optional_token(
            "content.tier_qualification_window",
            view.tier_qualification_window.as_deref(),
            price_repo::TIER_QUALIFICATION_WINDOWS,
            TierQualificationWindow::as_str,
        )?,
        aggregation_function: optional_token(
            "content.aggregation_function",
            view.aggregation_function.as_deref(),
            price_repo::AGGREGATION_FUNCTIONS,
            AggregationFunction::as_str,
        )?,
        aggregation_granularity: optional_token(
            "content.aggregation_granularity",
            view.aggregation_granularity.as_deref(),
            price_repo::AGGREGATION_GRANULARITIES,
            AggregationGranularity::as_str,
        )?,
        max_hold_granules: view.max_hold_granules,
        included_allowance: view
            .included_allowance
            .as_ref()
            .map(|allowance| {
                Ok::<_, DomainError>(IncludedAllowance {
                    quantity: allowance.quantity,
                    rollover_policy: wire_token(
                        "content.included_allowance.rollover_policy",
                        &allowance.rollover_policy,
                        price_repo::ROLLOVER_POLICIES,
                        RolloverPolicy::as_str,
                    )?,
                })
            })
            .transpose()?,
        reserved_rate: view
            .reserved_rate_nano_minor
            .map(RateMinor::from_nano_minor)
            .transpose()?,
        reservation_flavor: optional_token(
            "content.reservation_flavor",
            view.reservation_flavor.as_deref(),
            price_repo::RESERVATION_FLAVORS,
            ReservationFlavor::as_str,
        )?,
        min_qty_purchase: view.min_qty_purchase,
        min_qty_usage: view.min_qty_usage,
        min_qty_usage_fallback: optional_token(
            "content.min_qty_usage_fallback",
            view.min_qty_usage_fallback.as_deref(),
            price_repo::MIN_QTY_USAGE_FALLBACKS,
            MinQtyUsageFallback::as_str,
        )?,
        discount_ref: view.discount_ref.clone(),
    };
    Ok(PriceContent {
        row,
        tax_inclusive: view.tax_inclusive.unwrap_or(false),
        // Blank is refused rather than stored: an empty category is a caller
        // mistake naming one field, and letting it reach the column would make
        // "states none" and "states the empty string" two spellings of one fact
        // — which is the ambiguity `pricing_price` declines to encode.
        tax_category_ref: match view.tax_category_ref.as_deref().map(str::trim) {
            Some("") => {
                return Err(DomainError::InvalidRequest(
                    "content.tax_category_ref must not be blank: omit it to state no \
                     category, which resolves the region's default at publish (D-154)"
                        .to_owned(),
                ));
            }
            other => other.map(ToOwned::to_owned),
        },
        billing_timing: view.billing_timing.clone(),
        proration_contract: read_proration_contract(view)?,
        rounding_policy_ref: view.rounding_policy_ref.clone(),
        grandfather_until: view.grandfather_until,
        supersedes_price_id: view.supersedes_price_id,
    })
}

/// Read the four authored proration fields into the one value that holds them
/// (`06-consumer-contracts.md` §6, `inst-pi-required`).
///
/// **All three or none.** The contract is required as a set on a recurring row,
/// so the only two states worth accepting are a whole one and an absent one; a
/// caller sending two of three has made a mistake this surface can name, and
/// admitting it would push a partial set at the publish rules, which report it
/// as a missing contract and lose which field the caller forgot.
///
/// It is a `400` and not a publish violation for the reason every other refusal
/// in this function is: an unreadable request is not a plan that fails to
/// publish. Absence of the whole set is **not** refused here — that is
/// `inst-pi-required`'s call at publish, and a draft is allowed to be
/// unfinished.
///
/// The `fixed_day` pairing is enforced in both directions, because
/// [`BillingAnchorPolicy`] cannot hold a mismatched one and this is the boundary
/// where an unrepresentable state has to be refused rather than dropped.
fn read_proration_contract(
    view: &PriceContentView,
) -> Result<Option<ProrationContract>, DomainError> {
    let present = [
        view.billing_anchor_policy.is_some(),
        view.proration_basis.is_some(),
        view.credit_on_downgrade.is_some(),
    ];
    if present.iter().all(|p| !p) {
        if view.anchor_day.is_some() {
            return Err(DomainError::InvalidRequest(
                "content.anchor_day was sent without content.billing_anchor_policy: the day \
                 belongs to a fixed_day anchor and means nothing on its own"
                    .to_owned(),
            ));
        }
        return Ok(None);
    }
    let (Some(policy_token), Some(basis_token), Some(credit_on_downgrade)) = (
        view.billing_anchor_policy.as_deref(),
        view.proration_basis.as_deref(),
        view.credit_on_downgrade,
    ) else {
        return Err(DomainError::InvalidRequest(
            "content.billing_anchor_policy, content.proration_basis and \
             content.credit_on_downgrade publish as a set on a recurring row and must be sent \
             together; omit all three to leave the draft unfinished"
                .to_owned(),
        ));
    };

    // Through the enum's own roster, exactly as the `ProrationBasis` field two
    // lines below already did. This match used to re-spell all three tokens and
    // the refusal message re-spelled them a fourth time, in a module that imports
    // the enum — so a fourth K2 policy was a legal request refused as unknown,
    // with nothing reddening.
    let policy = wire_token(
        "content.billing_anchor_policy",
        policy_token,
        BillingAnchorPolicy::ALL,
        BillingAnchorPolicy::as_str,
    )?;
    // The day is the second fact, and the pairing is where the two unpublishable
    // spellings are refused: a `fixed_day` with no day, and a day beside a policy
    // that anchors without one. `with_anchor_day` deliberately does not refuse
    // either — only this caller knows whether a day was *sent*.
    let billing_anchor_policy = match policy.anchor_day() {
        Some(_) => {
            let day = view.anchor_day.ok_or_else(|| {
                DomainError::InvalidRequest(
                    "content.anchor_day is required beside a fixed_day anchor".to_owned(),
                )
            })?;
            policy.with_anchor_day(
                AnchorDay::new(day).map_err(|e| DomainError::InvalidRequest(e.to_string()))?,
            )
        }
        None => none_anchored(policy, view)?,
    };

    Ok(Some(ProrationContract {
        billing_anchor_policy,
        proration_basis: wire_token(
            "content.proration_basis",
            basis_token,
            ProrationBasis::ALL,
            ProrationBasis::as_str,
        )?,
        credit_on_downgrade,
    }))
}

/// A policy that names no day, with the day refused if one was sent.
fn none_anchored(
    policy: BillingAnchorPolicy,
    view: &PriceContentView,
) -> Result<BillingAnchorPolicy, DomainError> {
    if view.anchor_day.is_some() {
        return Err(DomainError::InvalidRequest(format!(
            "content.anchor_day is only authorable beside a fixed_day anchor; this row anchors \
             {}",
            policy.as_str()
        )));
    }
    Ok(policy)
}

/// Refuse what Slice 10 has not landed: the qualification window, and the
/// **`carry`** half of the allowance.
///
/// # The window, at any value
///
/// The refusal is **not value-conditioned**: `inst-tt-forbidden` refuses an
/// *explicit* window of any value, `current` included, so a check that only
/// caught `trailing_period` would accept the default spelled out and store a
/// field nothing judges.
///
/// # The allowance, narrowed to `carry`
///
/// `includedAllowance {N, none}` is **authorable now**: `inst-ac-gate`'s six
/// refusals judge it row-locally at this very write
/// ([`crate::domain::rules::allowance`]) and publish compiles it
/// ([`crate::domain::allowance`]), so the value has both a rule and a meaning —
/// the two things D-177 clause (3) required before the refusal could go.
///
/// `carry` does not, and the reason is a store rather than a rule: its artifact
/// is a `promotional` grant row in `pricing_plan_grant` (`inst-ac-carry`, D-52),
/// a table this gear does not have. Accepting one would freeze a carry horizon
/// nothing ever issues — the allowance would simply not arrive, at zero price
/// delta and with no alarm, which is the D-129 failure mode with a different
/// cause. So it keeps D-177's posture and
/// [`unjudged_primitives`](crate::domain::publish::rules::unjudged_primitives)
/// keeps the matching publish backstop.
///
/// The policy is compared against the wire token rather than a parsed one
/// because this runs **before** the row is built, deliberately: the refusal is
/// about the field, so it must precede the enum-spelling check that would
/// otherwise tell a caller their `carry` was misspelled. A token that is neither
/// `none` nor `carry` still reaches `wire_token` below and is refused there.
///
/// The message names the field and says why, because the caller's next action
/// differs from every other 400 on this surface: there is nothing to correct in
/// the request, the primitive is unsupported.
fn refuse_unlanded_primitives(view: &PriceContentView) -> Result<(), DomainError> {
    if view.tier_qualification_window.is_some() {
        return Err(DomainError::InvalidRequest(
            "content.tier_qualification_window is not supported yet: the gear can store the value \
             and cannot judge it. Slice 10's refusals (TIER_QUAL_ON_NON_TIERED, \
             TIER_QUAL_WINDOW_INCOMPATIBLE, TIER_QUAL_ZERO_BAND_LOCK, FIXTURE_MISSING) and the \
             trailing-tier fixture the window's publish block needs have not landed, so an \
             accepted window would be an authoring error nothing reports. Omit the field"
                .to_owned(),
        ));
    }
    if view
        .included_allowance
        .as_ref()
        .is_some_and(|allowance| allowance.rollover_policy == RolloverPolicy::Carry.as_str())
    {
        return Err(DomainError::InvalidRequest(
            "content.included_allowance with rollover_policy = carry is not supported yet: the \
             gear can store the declaration and cannot issue it. inst-ac-carry materializes a \
             per-period promotional grant into pricing_plan_grant, a table this gear does not \
             have, so a carried allowance would be frozen into an immutable version and never \
             granted. rollover_policy = none is authorable: it compiles to the $0 band, the \
             offset ladder and the display marker"
                .to_owned(),
        ));
    }
    Ok(())
}

/// One band. An absent `to_qty` is the **open** top, not a missing value.
fn band_of(view: &TierBandView) -> Result<TierBand, DomainError> {
    Ok(TierBand {
        from_qty: view.from_qty,
        to_qty: view.to_qty.map_or(BandTop::Open, BandTop::Closed),
        unit_price_rate: RateMinor::from_nano_minor(view.unit_price_nano_minor)?,
    })
}

/// An amount, refused rather than coerced when it is negative: typed credit rows
/// are deliberately out of scope, so a negative price is a mistake.
fn amount(field: &str, raw: Option<i64>) -> Result<Option<MinorAmount>, DomainError> {
    raw.map(|value| {
        MinorAmount::new(value).map_err(|e| match e {
            DomainError::AmountNegative(_) => {
                DomainError::AmountNegative(format!("{field}: {value}"))
            }
            other => other,
        })
    })
    .transpose()
}

/// Read an optional wire token.
///
/// `pub(crate)` for one second caller,
/// [`crate::api::rest::repricing_runs`], whose selector is nine optional axes and
/// two of them are enumerations. Shared rather than copied for
/// [`scope_key_of`]'s reason: the refusal's sentence enumerates the admitted
/// tokens from the same `candidates` slice the store's `CHECK` was written
/// against, so a token added on one plane cannot be missing from the other's
/// message.
pub(crate) fn optional_token<T: Copy>(
    field: &str,
    token: Option<&str>,
    candidates: &[T],
    render: fn(T) -> &'static str,
) -> Result<Option<T>, DomainError> {
    token
        .map(|token| wire_token(field, token, candidates, render))
        .transpose()
}

/// Read a wire token back into the domain value that renders it.
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
#[path = "prices_tests.rs"]
mod prices_tests;
