//! The `SnapshotSynthesizer` of §1.7 — the reads behind D-76's two tiers and
//! D-87's self-contained payload (`inst-sy-freeze`, `inst-sy-select`,
//! `inst-sy-payload`, `inst-sy-provenance`, `inst-sy-backdate`, `inst-ms-synth`).
//!
//! [`crate::domain::synthesis`] holds the rule; this module holds the queries and
//! the freeze. The split is the usual one, and here it earns itself twice over:
//! tier 2's query has no store to run against, so keeping the *rule* free of that
//! fact is what lets the rule stay tested while the read is absent.
//!
//! # Tier 1, and the one reader it needs
//!
//! "The `pricing_price` row, **current or superseded**, whose `PriceWindow`
//! covered `t` on that key". Every part of that is on one record:
//! `window_repo::list_for_plan` resolves each window's ten-axis
//! [`ScopeKey`](crate::domain::scope_key::ScopeKey) from `pricing_price` on read,
//! so the key match, the interval test and the row id all come from one query and
//! there is no join to get wrong.
//!
//! **Half-open, `[effective_from, effective_to)`.** An instant exactly at a
//! window's end belongs to the *next* window, which is the same rule coverage
//! uses, and `effective_to = None` is open-ended rather than missing.
//!
//! **Cancelled windows are excluded.** A cancelled window never took effect, so a
//! row it scheduled was never what rating resolved — including it would let
//! synthesis freeze a price the subscriber demonstrably never paid. `scheduled`,
//! `active` and `expired` are all admitted: what makes a window evidence here is
//! that its interval covered `t`, not what state the clock has since moved it to.
//!
//! **And the row behind the window must be `published` or `superseded`**, which is
//! clause 1 read literally. A claim about *windows* cannot stand in for that filter,
//! because it has to be about *rows*: `list_for_plan` is taken whole, over every
//! price row of the plan whatever its lifecycle, so a `draft` row's window is
//! admissible evidence and a row that never published can be frozen — labelled
//! `live_history` — as what the subscriber was paying.
//!
//! # There is no tier 2, and no seam held open for one
//!
//! Treating `pricing_historical_price` as merely unbuilt — [`select_for_key`]
//! passing an **empty** reference candidate set into the domain rule rather than not
//! calling it, on the argument that "the day the store lands only the query behind
//! it changes" — is a promise nothing intends to keep. `inst-bd-store` and
//! `inst-sy-backdate`, the instruction naming synthesis the sanctioned **consumer**
//! of the backdating path, are both out of the design set.
//!
//! **D-330 struck the flow, the store and that instruction.** There is
//! no day when the store lands, so the parameter is deleted rather than left
//! standing as a promise nothing intends to keep — `select_rows` takes the live
//! candidates alone. What is untouched is tier 1 (D-330 clause 3) and the
//! per-row `source` discriminator the tier was recorded under, which S11 §4
//! clause 2 keeps at one value.
//!
//! # The payload materializes what nothing can look up
//!
//! D-87 plus C-5. Two halves, and the second is the one that was missing when the
//! rule was first written:
//!
//! * **per resolved row** — the evaluable content, read off `pricing_price` and
//!   `pricing_price_tier_band`. The band table is the second read and it is not
//!   optional: `check_amount_placement` forbids **both** scalar money columns on
//!   a `graduated` / `volume` row, so a payload built from `pricing_price` alone
//!   priced those two kinds at nothing at all;
//! * **plan-level** — the billing descriptor set and the resolved entitlement
//!   grant set, without which the payload is row-complete and invoice-incomplete,
//!   because Billing has no `CatalogVersion` to fall back to on a
//!   `migrated-origin` ref by construction.
//!
//! **The grant set is empty and that is not a shortcut.** There is no entitlement
//! grant store in this gear — no table, no column, no domain type — so the
//! plan-level half lands descriptor-complete and grant-absent. It is reported
//! rather than silently omitted: the payload carries `grantSetUnavailable` so a
//! consumer cannot read the absence as "this plan grants nothing".

use sea_orm::{ColumnTrait, Condition, EntityTrait, Order};
use serde_json::{Value as JsonValue, json};
use toolkit_db::secure::{AccessScope, DBRunner, SecureEntityExt};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::scope_key::PlanId;
use crate::domain::synthesis::{
    LiveCandidate, SelectedRow, SynthesisOutcome, UnresolvedKey, select_rows,
};
use crate::domain::window::WindowState;
use crate::infra::storage::entity::{
    plan_descriptor_set, plan_period_floor_cap, price, price_tier_band,
};
use crate::infra::storage::repo::{plan_repo, price_repo, window_repo};
use crate::infra::storage::{RepoError, repo_failure};
use time::OffsetDateTime;

/// A scope key synthesis must resolve a row for — the subscription's frozen
/// `(currency, region)` pair (D-76).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrozenKey {
    /// ISO currency.
    pub currency: String,
    /// The region axis.
    pub region: String,
}

/// The plan's window plane and its admissible row set — the two reads every key
/// of one synthesis resolves against.
///
/// **Read once per synthesis rather than once per key.** Neither depends on the
/// key, and [`resolve`] walks every frozen
/// `(currency, region)` pair of the subscription: `list_for_plan` is itself two
/// statements (`load_scope_keys_for_plan` then `load_windows`) and
/// `load_for_plan` a third, so a subscription with K frozen keys issued 3·K
/// identical queries over the whole plan — inside the synthesis write
/// transaction, which on this crate's `SQLite` harness is the single connection
/// everything else is waiting on.
struct PlanPlane {
    windows: Vec<window_repo::WindowRecord>,
    admissible: Vec<crate::domain::price_record::PriceRecord>,
}

/// Read the plane once.
async fn plan_plane(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
) -> Result<PlanPlane, DomainError> {
    let windows = window_repo::list_for_plan(runner, scope, tenant_id, plan_id)
        .await
        .map_err(|e| repo_failure(&e))?;

    // **D-76 clause 1 is "the `pricing_price` row, *current or superseded*", and
    // that half was missing.** `list_for_plan` is taken whole — every window state,
    // over every price row of the plan whatever its lifecycle — so a `draft` row's
    // window was admissible evidence and synthesis could freeze, as "what the
    // subscriber was paying", a row that never published, never passed the publish
    // rules and was never approved. It labelled it `live_history` while doing so.
    //
    // `read_model::project_windows` is the other reader of this same list and the
    // other one that asserts fact to a consumer; it restricts on exactly this axis.
    // The intersection is done the same way, against rows read in
    // `PROJECTED_ROW_STATES` rather than by adding a lifecycle column to
    // `WindowRecord` — that type is shared by every window caller, and none of the
    // others is asking this question.
    let admissible = price_repo::load_for_plan(
        runner,
        scope,
        tenant_id,
        plan_id,
        crate::domain::projection::PROJECTED_ROW_STATES,
    )
    .await
    .map_err(|e| repo_failure(&e))?;

    Ok(PlanPlane {
        windows,
        admissible,
    })
}

/// Resolve one scope key against both tiers at `t` (`inst-sy-select`).
///
/// The read-free half, so [`resolve`] can drive it over one plane rather than
/// re-reading that plane per key. Every filter below is a property of the key and
/// the instant, which is why hoisting the reads changes nothing it decides.
fn select_against(plane: &PlanPlane, key: &FrozenKey, at: OffsetDateTime) -> Vec<SelectedRow> {
    let PlanPlane {
        windows,
        admissible,
    } = plane;
    let mut live: Vec<LiveCandidate> = windows
        .iter()
        .filter(|window| {
            // A cancelled window never took effect; see the module doc.
            window.state != WindowState::Cancelled
                && admissible
                    .iter()
                    .any(|row| row.price_id == window.price_id)
                && window.scope_key.currency().as_str() == key.currency
                && window.scope_key.region().as_str() == key.region
                // Half-open: `[from, to)`.
                && window.effective_from <= at
                && window.effective_to.is_none_or(|to| at < to)
        })
        .map(|window| LiveCandidate {
            price_id: window.price_id,
            plan_revision: None,
        })
        .collect();
    // Deterministic, so two runs of one synthesis cannot resolve different rows
    // where a key is covered by more than one admitted window.
    live.sort_by_key(|candidate| candidate.price_id);

    select_rows(&live)
}

/// Resolve one scope key against both tiers at `t` (`inst-sy-select`), reading
/// the plan's plane for it.
///
/// The single-key entry point. [`resolve`] does not go through it — it reads the
/// plane once and drives [`select_against`] per key — so this is the shape a
/// caller with exactly one key wants and nothing more.
///
/// # Errors
/// [`DomainError::Internal`] on a storage failure.
pub async fn select_for_key(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    key: &FrozenKey,
    at: OffsetDateTime,
) -> Result<Vec<SelectedRow>, DomainError> {
    let plane = plan_plane(runner, scope, tenant_id, plan_id).await?;
    Ok(select_against(&plane, key, at))
}

/// Resolve every frozen key of one subscription (`inst-sy-select`).
///
/// # Errors
/// [`DomainError::Internal`] on a storage failure.
pub async fn resolve(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    keys: &[FrozenKey],
    at: OffsetDateTime,
) -> Result<SynthesisOutcome, DomainError> {
    // Once for the whole subscription; see [`PlanPlane`]. Read even when `keys` is
    // empty, deliberately: the plane is what a later clause would judge an empty
    // key set against, and skipping the read would make that a second decision.
    let plane = plan_plane(runner, scope, tenant_id, plan_id).await?;

    let mut selected = Vec::new();
    let mut unresolved = Vec::new();
    for key in keys {
        // **Every live line on the market, not the first.** A `FrozenKey` is
        // D-76's `(currency, region)` pair, and a market legitimately carries
        // more than one — `inst-cs-hybrid` sanctions a recurring row beside a
        // usage row. An empty set is what fails the key closed.
        let rows = select_against(&plane, key, at);
        if rows.is_empty() {
            unresolved.push(UnresolvedKey {
                currency: key.currency.clone(),
                region: key.region.clone(),
            });
        } else {
            selected.extend(rows);
        }
    }
    Ok(SynthesisOutcome {
        selected,
        unresolved,
    })
}

/// Materialize D-87's self-contained payload for a resolved set.
///
/// # Errors
/// [`DomainError::Internal`] on a storage failure; [`DomainError::NotFound`] when
/// a resolved tier-1 row has vanished between selection and materialization,
/// which cannot happen inside one transaction and is reported rather than
/// unwrapped.
pub async fn materialize(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    selected: &[SelectedRow],
) -> Result<JsonValue, DomainError> {
    let mut rows = Vec::new();
    for row in selected {
        let stored = price::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(price::Column::TenantId.eq(tenant_id))
                    .add(price::Column::PriceId.eq(row.row_id)),
            )
            .one(runner)
            .await
            .map_err(|e| {
                repo_failure(&RepoError::Db(format!(
                    "materialize the migrated-origin payload of price row {}: {e}",
                    row.row_id
                )))
            })?
            .ok_or_else(|| DomainError::NotFound {
                subject: "price row".to_owned(),
                id: row.row_id.to_string(),
            })?;

        // **The band set, and it is the whole price of the row that has one.**
        // Read here by hand for the reason the descriptor set and the period
        // bounds are: this payload resolves through no `CatalogVersion`, so a
        // ladder outside it is one Billing can neither apply nor look up.
        // Ascending by lower bound, which is `inst-tb-order`'s single read-side
        // guarantee — the table carries no ordinal and authored order does not
        // survive it, so the order has to come from the query.
        let bands = price_tier_band::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(price_tier_band::Column::TenantId.eq(tenant_id))
                    .add(price_tier_band::Column::PriceId.eq(row.row_id)),
            )
            .order_by(price_tier_band::Column::FromQty, Order::Asc)
            .all(runner)
            .await
            .map_err(|e| {
                repo_failure(&RepoError::Db(format!(
                    "read the band set of price row {}: {e}",
                    row.row_id
                )))
            })?;

        rows.push(row_value(row, &stored, &bands));
    }

    Ok(json!({
        "rows": rows,
        "planLevel": plan_level(runner, scope, tenant_id, plan_id).await?,
        // Foundation §4.4 names a `migrated-origin` ref the one deliberately
        // non-version-pinned reference. Stated in the payload so a consumer that
        // looks for a version learns why there is none rather than failing.
        "catalogVersion": JsonValue::Null,
        "catalogVersionDeliberatelyAbsent": true,
    }))
}

/// One resolved row's evaluable content, **as authored**.
///
/// Split out of [`materialize`] so neither exceeds the line budget, and because
/// the two do different jobs: that function decides *which* rows the payload
/// carries and this one decides what a row *is*.
///
/// # Everything here is the stored column, and none of it is a compiled artifact
///
/// The rule this builder follows is the one D-323 set for the rate member and it
/// now governs the whole row: the payload renders what the operator authored,
/// and `projection::row_value`'s compiled members are deliberately not copied.
/// The read model materializes the D-45 compile — a presented `graduated` kind,
/// the `$0` band plus the offset ladder, a nulled rate and an `allowanceMarker`
/// — because a published row resolves through a `CatalogVersion` that froze it.
/// This record carries no compiled ladder at all, so copying the nulling would
/// take a row's only price away, and adding the marker would put the *first*
/// compiled artifact into a payload whose `includedAllowance` is the authored
/// declaration beside it.
fn row_value(
    row: &SelectedRow,
    stored: &price::Model,
    bands: &[price_tier_band::Model],
) -> JsonValue {
    json!({
        "rowId": row.row_id,
        "source": row.tier.as_str(),
        "currency": stored.currency,
        "region": stored.region,
        "phase": stored.phase,
        "chargeKind": stored.charge_kind,
        "modelKind": stored.model_kind,
        "amountMinor": stored.amount_minor,
            // **The `per_unit` row's money, and between D-311 and D-323 it was
            // nowhere in this payload.** D-311 moved a `per_unit` rate out of
            // `amount_minor` into its own column and measured its cost as
            // references to `unit_price_minor`, the *band* rate; this builder
            // reads `amount_minor` and touches no band, so it is in neither that
            // list nor the staging, and went on rendering `amountMinor` alone.
            //
            // The absence is not conditional. `check_amount_placement` does not
            // merely allow `amount_minor` to be NULL on a `per_unit` row, it
            // **forbids** the column — "two priced columns are two competing
            // prices" — so every synthesized `per_unit` line carried
            // `"amountMinor": null` and no price at all, frozen INSERT-only into
            // a record that resolves through no `CatalogVersion` and can never
            // be corrected.
            //
            // **Rendered as authored**, unlike the read model's member of the
            // same name: `projection::row_value` nulls it on an
            // allowance-carrying row because the D-45 compile folds the rate into
            // the top band, and this payload carries no **compiled** ladder — the
            // `bands` member below is the authored set and this function's doc says
            // why the compile stays out — so the same nulling would take the row's
            // only price away a second time.
            //
            // No `*Unavailable` marker rides beside it, and the difference from
            // `grantSetUnavailable` is the point: that marker reports a set this
            // payload could not *read*, while this column is read on every row
            // and `modelKind` — which is right above — says which member holds
            // the price.
        "unitRateNanoMinor": stored.unit_rate_nano,
            // **The fourth priced member, and the one the placement matrix leaves as
            // the whole price of two model kinds.** `check_amount_placement` gives
            // `graduated` and `volume` `(wants_amount, wants_rate) = (false, false)`
            // and *forbids* either column, so a synthesized tiered line carried
            // `"amountMinor": null`, `"unitRateNanoMinor": null` and no ladder — no
            // price at all, frozen INSERT-only into a record that resolves through
            // no `CatalogVersion` and can never be corrected. Exactly D-323's defect
            // one class wider.
            //
            // **The authored ladder, not the compiled one** — see this function's
            // doc. `pricing_price_tier_band` holds authored bands only (D-130), so
            // "as stored" and "as authored" are the same read here.
            //
            // Rendered on **every** row, empty where the kind authors none, and no
            // `*Unavailable` marker beside it. That is `grantSetUnavailable`'s
            // distinction applied: those markers report a set the payload could not
            // *read*, and this one is read on every row. D-323 held its own marker
            // argument open for exactly this member — on a tiered row `modelKind`
            // pointed at a set the payload did not carry, so the absence was
            // unreadable — and rendering the set is what closes it: an empty array
            // beside `"modelKind": "flat"` says the row has no bands, which is what
            // `inst-mk-forbidden` says too.
        "bands": bands
            .iter()
            .map(|band| {
                json!({
                    "fromQty": band.from_qty,
                        // `null` is the **open top** (D-17) — a state of the band
                        // rather than an absent value, and the read model's
                        // spelling of it.
                    "toQty": band.to_qty,
                        // D-311's scale and the read model's member name, so one
                        // number has one name whichever door a consumer read it
                        // through.
                    "unitPriceNanoMinor": band.unit_price_nano,
                })
            })
            .collect::<Vec<_>>(),
        "packageSize": stored.package_size,
        "packagePriceMinor": stored.package_price_minor,
        "meter": stored.meter,
        // `null` for an absent line, which is what the member above it renders
        // for the other half of the same pair. `meter` is an `Option` column and
        // `dimension_key` is not, so copying both raw spells one absence two
        // ways inside one frozen document — and this plane is INSERT-only over a
        // record that resolves through no `CatalogVersion` and can never be
        // corrected.
        "dimensionKey": (!stored.dimension_key.is_empty()).then(|| stored.dimension_key.clone()),
            // The evaluation-policy and S6 consumer-contract fields: a
            // `migrated-origin` line is evaluated from this and nothing else.
            //
            // **Projected, not raw.** `stored.billing_timing` is `NULL` on a usage line by
            // intent — `inst-bt-required` requires it only on `recurring`, and
            // `check_setup_fields` forbids it on setup rows — so rendering the column
            // directly puts `null` on every usage and one-time line of a legacy
            // subscriber's frozen record, where the read model for the identical row
            // renders `arrears` / `advance`. `published_billing_timing`'s own doc says why
            // the column must
            // not be handed out: *"the constant wins over anything in the column …
            // were an authored value allowed to displace it, Billing's deferral on a
            // usage line would depend on whether someone had typed into a column the
            // design says is not theirs to author"* — and `projection.rs` names this
            // exact payload as the hazard. One rule, reached through
            // `ChargeKind::parse` rather than restated here.
            //
            // An unparseable token renders `null`, which is the pre-existing
            // behaviour for a row whose kind this build does not know.
        "billingTiming": crate::domain::scope_key::ChargeKind::parse(&stored.charge_kind)
            .and_then(|kind| {
                crate::domain::contracts::published_billing_timing(
                    kind,
                    stored.billing_timing.as_deref(),
                )
            }),
        "quantitySource": stored.quantity_source,
            // **What the source says, beside where it comes from.**
            // `check_quantity_source` does not merely permit `manual_quantity`
            // beside a `manual` source, it *requires* it — "the fixed quantity is
            // the whole of what `manual` supplies" — so the payload stated where a
            // non-usage `per_unit` row's `Q` comes from and never what it is, and
            // the rate D-323 restored had nothing to multiply.
        "manualQuantity": stored.manual_quantity,
        "billingGranularity": stored.billing_granularity,
        "aggregationFunction": stored.aggregation_function,
        "aggregationGranularity": stored.aggregation_granularity,
        "tierAggregationWindow": stored.tier_aggregation_window,
        "tierQualificationWindow": stored.tier_qualification_window,
            // The hold bound a level fold reads (`inst-la-hold`); required on a
            // non-`sum` row, so it is not an optional decoration there.
        "maxHoldGranules": stored.max_hold_granules,
        "includedAllowance": stored.included_allowance,
            // **This payload is uncompiled, and now says so**.
            //
            // The read model materializes the D-45 compile — a presented `graduated`
            // kind, the `[0, N) @ $0` band plus the offset ladder, a nulled rate and
            // an `allowanceMarker`. This door renders the row as authored, which is
            // this function's stated rule and D-323's, and the design set scopes AC
            // #90a's compile-equivalence to what a `pricingSnapshotRef` carries — not
            // to this deliberately unversioned record. So the rendering stays.
            //
            // What could not stay is the **silence**. A consumer reading
            // `modelKind: "per_unit"` with `unitRateNanoMinor: 23_000_000` and no
            // bands bills 150 GB at 150 x the rate, where the read model for the
            // identical row bills 50 — the 100 GB the subscriber's allowance grants,
            // every period, out of a record `freeze_or_load` makes uncorrectable. The
            // two doors rendered the same row in two shapes and nothing on the wire
            // distinguished them.
            //
            // **A fact about this door, not a predicate over the row.** Stating it
            // unconditionally is what keeps it honest: `Admissibility::of` answers
            // over a domain `PriceRow`, which this builder deliberately does not
            // construct, so any computed marker here would be a second, drifting
            // reading of when the compile applies. `includedAllowance` above is the
            // authored declaration and the whole input the compile needs.
        "allowanceCompiled": false,
            // **The rest of the S6 proration contract.** The comment above claimed
            // this set and only `billingTiming` had made it. `inst-pi-required`
            // makes all three mandatory on a recurring row precisely because
            // Subscriptions prorates from the frozen values and the catalog
            // substitutes no defaults; `inst-pi-credit-source` then reads
            // `creditOnDowngrade` on a plan change "from the subscriber's frozen
            // snapshot", which is this record and nothing else.
            //
            // `anchorDay` is the stored column rather than the read model's derived
            // value: the pairing is structural — the domain enum carries the day
            // inside the `fixed_day` variant and no other policy can hold one — so
            // the column is already NULL under every policy that anchors without a
            // day, and re-deriving it here would be a second answer to a question
            // storage has already answered.
        "billingAnchorPolicy": stored.billing_anchor_policy,
        "anchorDay": stored.anchor_day,
        "prorationBasis": stored.proration_basis,
        "creditOnDowngrade": stored.credit_on_downgrade,
            // The reservation pair (`inst-rv-attrs`). **Money**: Rating sources the
            // self-service reserved rate from the row, and on a `migrated-origin`
            // line there is no other row to source it from.
        "reservedRateNanoMinor": stored.reserved_rate_nano,
        "reservationFlavor": stored.reservation_flavor,
            // The typed floors and the discount hook, carried verbatim
            // (`inst-ft-both`, `inst-dr-boundary`). Enforcement is downstream —
            // Subscriptions at order time, Tariffs/Rating at eligibility, Promotions
            // for the instrument — so a consumer that cannot read them here cannot
            // enforce them at all.
        "minQtyPurchase": stored.min_qty_purchase,
        "minQtyUsage": stored.min_qty_usage,
        "minQtyUsageFallback": stored.min_qty_usage_fallback,
        "discountRef": stored.discount_ref,
            // Tax basis, and both halves of each resolution: what the author wrote
            // and what the publish resolved it to. A publish that wrote the resolution
            // over the authored column would carry `roundingPolicyRef` under an authored
            // name, and a consumer could not tell a row that named a policy from one
            // that fell back to the tenant default.
        "taxInclusive": stored.tax_inclusive,
        "taxCategoryRef": stored.tax_category_ref,
        "resolvedTaxCategory": stored.resolved_tax_category,
        "roundingPolicyRef": stored.rounding_policy_ref,
        "resolvedRoundingPolicy": stored.resolved_rounding_policy,
    })
}

/// C-5's plan-level half of the payload: the descriptor set and the grant set.
async fn plan_level(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
) -> Result<JsonValue, DomainError> {
    let current = plan_repo::load_current(runner, scope, tenant_id, plan_id)
        .await
        .map_err(|e| repo_failure(&e))?;
    let Some(current) = current else {
        // A fully-legacy key may have no plan revision at all (D-87). The payload
        // says so rather than omitting the half, because "absent" and "empty" are
        // different things to a party posting an invoice from it.
        return Ok(json!({
            "descriptorSetUnavailable": true,
            "grantSetUnavailable": true,
            "periodFloorCapsUnavailable": true,
            "reason": "the source plan has no current revision; a fully legacy key belongs to none",
        }));
    };

    let revision = i64::try_from(current.revision).map_err(|_| {
        DomainError::Internal(format!(
            "plan {plan_id} stands at revision {}, which no column can address",
            current.revision
        ))
    })?;
    // D-319's period floor/cap set, read here by hand for the reason the
    // descriptor set is: this payload resolves through **no** `CatalogVersion`
    // by construction, so a bound outside it is a bound Billing cannot apply and
    // cannot look up. Frozen, and therefore permanently — which is why the
    // omission would have been worse here than in the read model. The `false`
    // marker beside it is `grantSetUnavailable`'s discipline: an empty list must
    // not be readable as "this plan has no minimum" unless it is one.
    let bounds = plan_period_floor_cap::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(plan_period_floor_cap::Column::TenantId.eq(tenant_id))
                .add(plan_period_floor_cap::Column::PlanId.eq(plan_id.get()))
                .add(plan_period_floor_cap::Column::PlanRevision.eq(revision)),
        )
        .order_by(plan_period_floor_cap::Column::Currency, Order::Asc)
        .order_by(plan_period_floor_cap::Column::Region, Order::Asc)
        .all(runner)
        .await
        .map_err(|e| {
            repo_failure(&RepoError::Db(format!(
                "read the period floor/cap set of plan {plan_id}: {e}"
            )))
        })?;

    let descriptors = plan_descriptor_set::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(plan_descriptor_set::Column::TenantId.eq(tenant_id))
                .add(plan_descriptor_set::Column::PlanId.eq(plan_id.get()))
                .add(plan_descriptor_set::Column::PlanRevision.eq(revision)),
        )
        .one(runner)
        .await
        .map_err(|e| {
            repo_failure(&RepoError::Db(format!(
                "read the descriptor set of plan {plan_id}: {e}"
            )))
        })?;

    Ok(json!({
        "planRevision": current.revision,
            // D-48's three v1 descriptor-set fields. Billing posts the line from
            // these, having no `CatalogVersion` to fetch them from.
        "invoiceLineTemplate": descriptors.as_ref().and_then(|d| d.invoice_line_template.clone()),
        "glCode": descriptors.as_ref().and_then(|d| d.gl_code.clone()),
        "itemizationRule": descriptors.as_ref().and_then(|d| d.itemization_rule.clone()),
        "descriptorSetUnavailable": descriptors.is_none(),
            // **There is no entitlement grant store in this gear.** Reported rather
            // than rendered as an empty set, because a consumer must not read the
            // absence as "this plan grants nothing".
        "grantSet": JsonValue::Null,
        "grantSetUnavailable": true,
            // D-319. Rendered with the same members the read-model delta uses, so a
            // consumer reads one shape whichever door the snapshot came through.
        "periodFloorCaps": bounds
            .iter()
            .map(|bound| {
                json!({
                    "currency": bound.currency,
                    "region": bound.region,
                    "floorMinor": bound.floor_minor,
                    "capMinor": bound.cap_minor,
                })
            })
            .collect::<Vec<_>>(),
        "periodFloorCapsUnavailable": false,
    }))
}

/// The resolved set, rendered for the provenance record (`inst-sy-provenance`).
///
/// **The tier rides each id.** An auditor reconstructing a disputed legacy charge
/// must be able to tell a real published price from a governed backdated
/// reconstruction without re-running the lookup, and a per-snapshot tier could
/// not say that about a subscription whose keys resolved differently.
#[must_use]
pub fn resolved_json(selected: &[SelectedRow]) -> JsonValue {
    JsonValue::Array(
        selected
            .iter()
            .map(|row| {
                json!({
                    "rowId": row.row_id,
                    "source": row.tier.as_str(),
                    "planRevision": row.plan_revision,
                })
            })
            .collect(),
    )
}

// ---------------------------------------------------------------------------
// The service (`inst-sy-surface`, `inst-sy-freeze`, `inst-ms-synth`).
// ---------------------------------------------------------------------------

/// The `SnapshotSynthesizer` of §1.7.
///
/// `Clone` for [`crate::infra::retirement::RetirementService`]'s reason: the one
/// field is a handle rather than state. It requests **no** `CatalogVersion`, and
/// that is not an omission — D-87 makes a `migrated-origin` ref the one
/// deliberately non-version-pinned reference in the system, so there is no
/// version for it to request.
#[derive(Clone)]
pub struct SynthesisService {
    db: toolkit_db::DBProvider<toolkit_db::DbError>,
}

impl SynthesisService {
    /// Build the synthesizer over one provider.
    #[must_use]
    pub const fn new(db: toolkit_db::DBProvider<toolkit_db::DbError>) -> Self {
        Self { db }
    }

    /// Read one subscription's frozen snapshot (`inst-sy-surface`, D-102).
    ///
    /// # Errors
    /// [`DomainError::Internal`] on a storage failure.
    pub async fn load(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        subscription_ref: Uuid,
    ) -> Result<Option<crate::infra::storage::repo::ProvenanceRecord>, DomainError> {
        let conn = self.db.conn().map_err(|e| {
            DomainError::Internal(format!("bss-pricing: synthesis read connection: {e}"))
        })?;
        crate::infra::storage::repo::synthesis_repo::load(&conn, scope, tenant_id, subscription_ref)
            .await
            .map_err(|e| repo_failure(&e))
    }

    /// Synthesize and **freeze** one subscription's snapshot (`inst-sy-freeze`,
    /// `inst-sy-select`, `inst-sy-payload`, `inst-ms-synth`).
    ///
    /// Idempotent by §9: a subscription that already holds a snapshot is handed
    /// **that** one back rather than freezing a second at a second instant. The
    /// check is the store's unique index rather than a read here, because the two
    /// calls would otherwise race and D-81 gives the two triggers different `t`.
    ///
    /// # No production caller reaches this today
    ///
    /// The service is constructed in `module.rs` and held on `GovernanceState`, so
    /// the wiring reads as live; the only `.synthesize(` call sites are in
    /// `tests/rest_migrated_origin_snapshots.rs`. Nothing writes a provenance
    /// record in production, so the mounted read
    /// `GET /migrated-origin-snapshots/{subscriptionRef}` can only ever answer
    /// `404` — an entire storage, rule and REST surface that is inert while
    /// looking wired.
    ///
    /// What is owed is the trigger: the migration execution handshake that calls
    /// this at cutover. Until it lands, every defect on this path is exercised by
    /// tests alone, which is the reason the gap is stated here rather than left to
    /// a `grep`.
    ///
    /// # Errors
    /// [`DomainError::PriceRowAbsent`] when any frozen key resolves through
    /// neither tier — clause (3)'s fail-closed refusal, which is what puts the
    /// subscription on the exception list; [`DomainError::Internal`] on a storage
    /// failure.
    pub async fn synthesize(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        request: SynthesisRequest,
    ) -> Result<crate::infra::storage::repo::synthesis_repo::Frozen, DomainError> {
        let scope = scope.clone();
        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<crate::infra::storage::repo::synthesis_repo::Frozen, DomainError, _>(
                move |txn| {
                    Box::pin(async move { synthesize_in(txn, &scope, tenant_id, request).await })
                },
            )
            .await;
        outcome.map_err(|err| {
            err.into_domain(|infra| {
                DomainError::Internal(format!("bss-pricing: synthesis transaction: {infra}"))
            })
        })
    }
}

/// What a caller must supply to synthesize a snapshot.
#[derive(Clone, Debug)]
pub struct SynthesisRequest {
    /// The subscription with no `pricingSnapshotRef`.
    pub subscription_ref: Uuid,
    /// The plan it is on.
    pub source_plan_id: PlanId,
    /// Its frozen `(currency, region)` keys.
    pub keys: Vec<FrozenKey>,
    /// D-81's instant `t`, chosen by the trigger.
    pub at: OffsetDateTime,
    /// Which trigger this is.
    pub trigger: crate::domain::synthesis::SynthesisTrigger,
    /// Who is acting — recorded on the provenance.
    pub acting_principal: Uuid,
}

/// One synthesis, in the caller's transaction.
///
/// The order is the rule's own: resolve every key, **refuse if any is
/// unresolved** (clause 3 — a partial snapshot is the one outcome that must not
/// exist), materialize, then freeze.
///
/// # Errors
/// See [`SynthesisService::synthesize`].
pub async fn synthesize_in(
    txn: &toolkit_db::secure::DbTx<'_>,
    scope: &AccessScope,
    tenant_id: Uuid,
    request: SynthesisRequest,
) -> Result<crate::infra::storage::repo::synthesis_repo::Frozen, DomainError> {
    let outcome = resolve(
        txn,
        scope,
        tenant_id,
        request.source_plan_id,
        &request.keys,
        request.at,
    )
    .await?;
    outcome.ensure_complete(request.subscription_ref)?;

    let payload = materialize(
        txn,
        scope,
        tenant_id,
        request.source_plan_id,
        &outcome.selected,
    )
    .await?;

    // **The revision the payload was frozen from**, read from the source plan
    // rather than from the selected rows.
    //
    // The rows cannot answer it: every `LiveCandidate` is built with
    // `plan_revision: None` and `select_rows` copies the field through, so a
    // `find_map` over them is structurally always `None` — and on an INSERT-only
    // store with a seven-year horizon that leaves an auditor no way to tell which
    // revision a disputed legacy charge was frozen from. `materialize` above
    // resolves the same plan in this transaction, so the two cannot disagree.
    //
    // `None` stays the honest answer for a fully-legacy key whose source plan has
    // no current revision at all — D-87's fact, and the same arm `plan_level`
    // takes one screen up.
    let source_revision = plan_repo::load_current(txn, scope, tenant_id, request.source_plan_id)
        .await
        .map_err(|e| repo_failure(&e))?
        .map(|current| current.revision);

    crate::infra::storage::repo::synthesis_repo::freeze_or_load(
        txn,
        scope,
        crate::infra::storage::repo::NewProvenance {
            provenance_id: Uuid::now_v7(),
            tenant_id,
            subscription_ref: request.subscription_ref,
            source_plan_id: request.source_plan_id,
            source_revision,
            snapshot_instant: request.at,
            trigger: request.trigger,
            acting_principal: request.acting_principal,
            resolved: resolved_json(&outcome.selected),
            payload,
            created_at: request.at,
        },
    )
    .await
    .map_err(|e| repo_failure(&e))
}
