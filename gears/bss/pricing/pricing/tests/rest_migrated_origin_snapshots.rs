//! Synthesis end to end, and `GET
//! /bss-pricing/v1/migrated-origin-snapshots/{subscriptionRef}` over the wire
//! (`inst-sy-freeze`, `inst-sy-select`, `inst-sy-payload`, `inst-sy-provenance`,
//! `inst-sy-surface`, `inst-sy-firstrating`, D-76, D-81, D-87, D-102).
//!
//! The domain suite proves the **rule** — tier order, the fail-closed clause, the
//! trigger tokens. What only this file can prove is the **reader**: that tier 1
//! actually finds the `pricing_price` row whose window covered `t`, and that it
//! stops finding it on the other side of the interval. Those are properties of a
//! query over real windows, and no unit test over hand-built candidates can see
//! them — a reader that ignored `effective_from` entirely would pass every case
//! in `domain::synthesis`.
//!
//! # The covering window is the publish path's own, and that was a finding
//!
//! This file first seeded one with `rest_support::seed_window`. Every case
//! reddened with `WindowOverlap` — a **driver** refusal rather than an assertion,
//! which is the signal that a guard exists that the fixture did not know about.
//! It does: `Harness::publish_price` already schedules `[2099-08-04,
//! 2099-09-01)` on the published row, so the extra open-ended window collided
//! with it on the same canonical scope key. The fixture uses the real one
//! instead. Both instants below are therefore facts about a window the
//! production path created, not about one the test invented, and neither ages -
//! `rest_support`'s standing rule is that fixture windows are dated 2099 so no
//! wall clock reaches them and no activation sweep moves them.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;
mod rest_support;

use axum::http::StatusCode;
use bss_pricing::api::rest::migrated_origin_snapshots::MIGRATED_ORIGIN_SNAPSHOT;
use bss_pricing::domain::error::DomainError;
use bss_pricing::domain::money::RateMinor;
use bss_pricing::domain::money::{CurrencyCode, MinorAmount};
use bss_pricing::domain::plan_shape::PeriodFloorCap;
use bss_pricing::domain::price_row::TierBand;
use bss_pricing::domain::scope_key::{PlanId, Region};
use bss_pricing::domain::synthesis::SynthesisTrigger;
use bss_pricing::infra::synthesis::{FrozenKey, SynthesisRequest};

use bss_pricing::domain::instant::utc_ymd_hms;
use rest_support::{
    Harness, body_json, problem_family, request, seed_publishable_manual_quantity_plan,
    seed_publishable_per_unit_plan, seed_publishable_plan, seed_publishable_tiered_usage_plan,
    seed_stamp,
};
use time::OffsetDateTime;
use uuid::Uuid;

const RATING_SERVICE: Uuid = Uuid::from_u128(0x_4a_71_46);

fn path(subscription_ref: Uuid) -> String {
    MIGRATED_ORIGIN_SNAPSHOT.replace("{subscriptionRef}", &subscription_ref.to_string())
}

/// Inside `[2099-08-04, 2099-09-01)` — the window the publish path schedules.
fn covered_at() -> OffsetDateTime {
    utc_ymd_hms(2099, 8, 15, 0, 0, 0)
}

/// A second instant inside the same window, for the idempotency case.
fn also_covered_at() -> OffsetDateTime {
    utc_ymd_hms(2099, 8, 20, 0, 0, 0)
}

/// **Before** the window opens, by seven weeks. Deliberately just outside rather
/// than a century away: what is under test is the interval bound, and a distant
/// instant would pass against a reader that compared nothing at all.
fn before_the_window() -> OffsetDateTime {
    utc_ymd_hms(2099, 6, 15, 0, 0, 0)
}

/// The key `rest_support`'s publishable seed files its row under.
fn seeded_key() -> FrozenKey {
    FrozenKey {
        currency: "EUR".to_owned(),
        region: "eu".to_owned(),
    }
}

/// A published plan whose one price row carries the publish path's own window.
async fn covered_plan(h: &Harness) -> Uuid {
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_plan(h, plan_id).await;
    h.publish(plan_id, seeded.revision).await;
    // This schedules `[2099-08-04, 2099-09-01)` on the row. See the module doc.
    h.publish_price(plan_id, seeded.price_id).await;
    plan_id
}

fn synthesis_request(subscription: Uuid, plan_id: Uuid, at: OffsetDateTime) -> SynthesisRequest {
    SynthesisRequest {
        subscription_ref: subscription,
        source_plan_id: PlanId::new(plan_id),
        keys: vec![seeded_key()],
        at,
        trigger: SynthesisTrigger::Migration,
        acting_principal: RATING_SERVICE,
    }
}

// ---------------------------------------------------------------------------
// The reader — tier 1 over real windows.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_instant_the_window_covers_resolves_from_live_history() {
    // D-76 tier 1: the `pricing_price` row whose `PriceWindow` covered `t`. This
    // reproduces what rating would have resolved and needs no import at all.
    let h = Harness::new().await;
    let plan_id = covered_plan(&h).await;
    let subscription = Uuid::now_v7();

    let frozen = h
        .governance
        .synthesis
        .synthesize(
            &h.scope(),
            h.tenant,
            synthesis_request(subscription, plan_id, covered_at()),
        )
        .await
        .expect("the instant is covered");

    assert!(frozen.created);
    assert_eq!(frozen.record.subscription_ref, subscription);
    assert_eq!(frozen.record.snapshot_instant, covered_at());
    assert_eq!(frozen.record.trigger, SynthesisTrigger::Migration);

    // `inst-sy-provenance`: the tier rides each resolved id, so an auditor can
    // tell a real published price from a governed reconstruction.
    let resolved = frozen.record.resolved.as_array().expect("an array");
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0]["source"], "live_history");
}

#[tokio::test]
async fn an_instant_before_the_window_fails_closed_and_never_takes_the_current_row() {
    // Clause (3), and the property the whole rule exists for: the current row is
    // precisely the price the subscriber was **not** paying. A reader that
    // ignored `effective_from` would resolve it here and this case is what says
    // so.
    let h = Harness::new().await;
    let plan_id = covered_plan(&h).await;

    let err = h
        .governance
        .synthesis
        .synthesize(
            &h.scope(),
            h.tenant,
            synthesis_request(Uuid::now_v7(), plan_id, before_the_window()),
        )
        .await
        .expect_err("the instant is before the window opens");

    let rendered = format!("{err:?}");
    assert!(rendered.contains("EUR"), "{rendered}");
    assert!(rendered.contains("were not paying"), "{rendered}");
}

/// **D-144's quantum on `snapshotInstant`** — the third authored-instant plane the
/// gate `repo.rs` calls "one rule" did not reach.
///
/// D-81's `t` is supplied by the trigger, carried back in a contract field
/// (`MigratedOriginSnapshotView.snapshot_instant`) and compared against window
/// bounds to select the rows, which is the whole of the rule's scope. Refused
/// rather than truncated, for D-144's reason: the caller can correct one value,
/// and a truncating freeze would resolve a different `t` than the one it recorded.
///
/// The instant is 137 microseconds inside the covering window, so the resolution
/// itself succeeds and the refusal can only be the gate — a value outside the
/// window would have been refused by the reader and proved nothing.
#[tokio::test]
async fn a_snapshot_instant_finer_than_the_quantum_is_refused() {
    let h = Harness::new().await;
    let plan_id = covered_plan(&h).await;
    let subscription = Uuid::now_v7();
    let unquantized = covered_at() + time::Duration::microseconds(137);

    let err = h
        .governance
        .synthesis
        .synthesize(
            &h.scope(),
            h.tenant,
            synthesis_request(subscription, plan_id, unquantized),
        )
        .await
        .expect_err("a sub-millisecond snapshot instant must be refused");
    let rendered = format!("{err:?}");
    assert!(
        rendered.contains("snapshotInstant"),
        "the refusal names the field the caller corrects: {rendered}"
    );
    assert!(
        rendered.contains("TimestampPrecisionExceeded"),
        "and it is the precision refusal, not a resolution failure: {rendered}"
    );

    // Nothing was frozen, so the subscription is still free to be synthesized at
    // the quantum — the same instant with its microseconds cleared.
    assert!(
        h.governance
            .synthesis
            .load(&h.scope(), h.tenant, subscription)
            .await
            .expect("load")
            .is_none()
    );
    h.governance
        .synthesis
        .synthesize(
            &h.scope(),
            h.tenant,
            synthesis_request(subscription, plan_id, covered_at()),
        )
        .await
        .expect("clearing the sub-millisecond digits is the whole remedy");
}

#[tokio::test]
async fn a_key_the_plan_does_not_publish_fails_closed() {
    // The key axis, not the time axis: the plan sells on `(EUR, eu)` and nothing
    // else, so a subscription frozen onto another market resolves nothing.
    let h = Harness::new().await;
    let plan_id = covered_plan(&h).await;

    let mut request = synthesis_request(Uuid::now_v7(), plan_id, covered_at());
    request.keys = vec![FrozenKey {
        currency: "JPY".to_owned(),
        region: "apac".to_owned(),
    }];

    // **The key, not merely an `Err`.** `synthesize` refuses on this path for
    // several unrelated reasons - the D-144 precision gate, a region taxonomy
    // declaring nothing, a store failure - so `is_err()` stays green against a
    // build in which the key axis stopped being checked and something else
    // refused instead.
    match h
        .governance
        .synthesis
        .synthesize(&h.scope(), h.tenant, request)
        .await
    {
        Err(DomainError::PriceRowAbsent(detail)) => assert!(
            detail.contains("(JPY, apac)"),
            "the refusal must name the key it could not resolve: {detail}"
        ),
        Err(other) => panic!("the key is unpublished, so the refusal is D-76's: {other:?}"),
        Ok(frozen) => panic!(
            "a key the plan does not publish has no evidence and must fail closed; it froze: {:?}",
            frozen.record.resolved
        ),
    }
}

#[tokio::test]
async fn one_uncovered_key_refuses_the_whole_snapshot() {
    // Partial synthesis is the one outcome that must not exist: a snapshot
    // missing a key is a subscription that will fail to rate on it later, with a
    // frozen record asserting its economics were captured.
    let h = Harness::new().await;
    let plan_id = covered_plan(&h).await;
    let subscription = Uuid::now_v7();

    let mut request = synthesis_request(subscription, plan_id, covered_at());
    request.keys = vec![
        seeded_key(),
        FrozenKey {
            currency: "JPY".to_owned(),
            region: "apac".to_owned(),
        },
    ];

    // The count and the roster together are what say "the whole snapshot": a
    // refusal naming *both* keys would mean the covered one had also failed to
    // resolve, and this case would read as green on a reader that resolves
    // nothing at all.
    match h
        .governance
        .synthesis
        .synthesize(&h.scope(), h.tenant, request)
        .await
    {
        Err(DomainError::PriceRowAbsent(detail)) => {
            assert!(
                detail.contains("(JPY, apac)"),
                "the refusal must name the uncovered key: {detail}"
            );
            assert!(
                !detail.contains("(EUR, eu)"),
                "the covered key resolved; only the uncovered one is named: {detail}"
            );
            assert!(
                detail.contains("1 scope key(s)"),
                "exactly one key went unresolved: {detail}"
            );
        }
        Err(other) => panic!("one uncovered key refuses through D-76's clause: {other:?}"),
        Ok(frozen) => panic!(
            "partial synthesis is the one outcome that must not exist; it froze: {:?}",
            frozen.record.resolved
        ),
    }

    // ...and nothing was frozen.
    assert!(
        h.governance
            .synthesis
            .load(&h.scope(), h.tenant, subscription)
            .await
            .expect("load")
            .is_none()
    );
}

#[tokio::test]
async fn a_cancelled_window_is_not_evidence_and_the_key_fails_closed() {
    // **This case exists because a probe found nothing.** Deleting the
    // `state != Cancelled` conjunct reddened not one test in this file: every
    // fixture window was `scheduled`, so the exclusion was never exercised.
    //
    // The rule matters more than its size suggests. A cancelled window never took
    // effect, so the row it scheduled was never what rating resolved — freezing
    // from it would put a price in the snapshot that the subscriber demonstrably
    // never paid, which is the same failure clause (3) refuses in the other
    // direction.
    let h = Harness::new().await;
    let plan_id = covered_plan(&h).await;

    // Cancel the window the publish path scheduled. `scheduled -> cancelled` is
    // one of §4's sanctioned edges, so this is a real transition rather than a
    // hand-written row.
    let conn = h.db.conn().expect("conn");
    let windows = bss_pricing::infra::storage::repo::window_repo::list_for_plan(
        &conn,
        &h.scope(),
        h.tenant,
        PlanId::new(plan_id),
    )
    .await
    .expect("list the plan's windows");
    assert_eq!(windows.len(), 1, "the publish path schedules exactly one");

    let (_, cancelled) =
        h.db.db()
            .in_transaction::<(), bss_pricing::infra::storage::RepoError, _>({
                let scope = h.scope();
                let tenant = h.tenant;
                let window_id = windows[0].window_id;
                move |txn| {
                    Box::pin(async move {
                        bss_pricing::infra::storage::repo::window_repo::transition(
                            txn,
                            &scope,
                            tenant,
                            window_id,
                            bss_pricing::domain::window::WindowState::Cancelled,
                            covered_at(),
                            rest_support::stamp_of(RATING_SERVICE, covered_at()),
                        )
                        .await
                        .map(|_| ())
                    })
                }
            })
            .await;
    cancelled.expect("the scheduled window cancels");

    // The instant is still inside the cancelled window's interval. Only the
    // exclusion stands between that and a frozen price nobody ever paid.
    match h
        .governance
        .synthesis
        .synthesize(
            &h.scope(),
            h.tenant,
            synthesis_request(Uuid::now_v7(), plan_id, covered_at()),
        )
        .await
    {
        // The seeded key is the *only* key in the request, so naming it is what
        // says the cancellation removed this window's evidence rather than the
        // request having been malformed or the plan unreadable.
        Err(DomainError::PriceRowAbsent(detail)) => assert!(
            detail.contains("(EUR, eu)"),
            "a cancelled window is not evidence, and the refusal names the key: {detail}"
        ),
        Err(other) => panic!("a cancelled window leaves the key uncovered: {other:?}"),
        Ok(frozen) => panic!(
            "a cancelled window never took effect and must not be evidence; it froze: {:?}",
            frozen.record.resolved
        ),
    }
}

// ---------------------------------------------------------------------------
// The freeze, and Section 9's idempotency.
// ---------------------------------------------------------------------------

/// **A draft row's window is not evidence** — D-76 clause 1 is "the `pricing_price`
/// row, **current or superseded**", and a `draft` row is neither.
///
/// `window_repo::list_for_plan` is taken whole: every window state, over every
/// price row of the plan *whatever its lifecycle state*. The filter tested
/// cancelled-ness, currency, region and the interval, and said nothing about the
/// row behind the window — so synthesis could freeze, as "what the subscriber was
/// paying", a row that never published, never passed the publish rules and was
/// never approved.
///
/// The module doc's "**Cancelled windows are excluded and nothing else is**" was
/// the claim that made it read as complete. It was about *windows*; the omission
/// is on the *row*.
///
/// `read_model::project_windows` — the other reader of this same
/// `list_for_plan`, and the one that asserts fact to a consumer — restricts on
/// exactly this axis (`PROJECTED_ROW_STATES`).
#[tokio::test]
async fn a_draft_rows_window_is_not_evidence_and_the_key_fails_closed() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_plan(&h, plan_id).await;
    // The plan revision publishes; **the row does not**. `seed_publishable_plan`
    // already schedules the coverage window `[2099-08-04, 2099-09-01)` on the row,
    // and `publish_price` is what would move the row itself to `published` — so
    // skipping it leaves exactly the state under test: a covering window over a
    // `draft` row, which the store admits and the reader has to refuse on its own.
    h.publish(plan_id, seeded.revision).await;

    let outcome = h
        .governance
        .synthesis
        .synthesize(
            &h.scope(),
            h.tenant,
            synthesis_request(Uuid::now_v7(), plan_id, covered_at()),
        )
        .await;

    match outcome {
        Err(err) => {
            let rendered = err.to_string();
            assert!(
                rendered.contains("EUR") || rendered.contains("eu"),
                "the refusal must name the key it could not resolve: {rendered}"
            );
        }
        Ok(frozen) => panic!(
            "a draft row is not 'current or superseded', so this key has no evidence and the \
             snapshot must fail closed; it froze: {:?}",
            frozen.record.resolved
        ),
    }
}

#[tokio::test]
async fn a_second_synthesis_returns_the_same_frozen_ref_at_the_same_instant() {
    // §9: "a second synthesis attempt is idempotent (same frozen ref)". The
    // second call names a **different** instant, and it must not take: D-81 gives
    // the two triggers different `t`, so a re-freeze would leave the subscription
    // with two different frozen prices and no rule saying which rating reads.
    let h = Harness::new().await;
    let plan_id = covered_plan(&h).await;
    let subscription = Uuid::now_v7();

    let first = h
        .governance
        .synthesis
        .synthesize(
            &h.scope(),
            h.tenant,
            synthesis_request(subscription, plan_id, covered_at()),
        )
        .await
        .expect("first");

    let mut second_request = synthesis_request(subscription, plan_id, also_covered_at());
    second_request.trigger = SynthesisTrigger::FirstRating;
    let second = h
        .governance
        .synthesis
        .synthesize(&h.scope(), h.tenant, second_request)
        .await
        .expect("second");

    assert!(first.created);
    assert!(!second.created, "the second attempt froze nothing");
    assert_eq!(second.record.provenance_id, first.record.provenance_id);
    assert_eq!(second.record.snapshot_instant, covered_at());
    assert_eq!(second.record.trigger, SynthesisTrigger::Migration);
}

/// **The frozen record names the revision the payload was frozen from.**
///
/// Read from the source plan, because the selected rows cannot answer it: every
/// `LiveCandidate` is built with `plan_revision: None` and `select_rows` copies the
/// field through, so a `find_map` over them is structurally always `None`. On an
/// INSERT-only store with a seven-year horizon that leaves an auditor no way to
/// tell which revision a disputed legacy charge was frozen from, and nothing else
/// in the record carries it.
#[tokio::test]
async fn the_frozen_record_names_the_source_plans_revision() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_plan(&h, plan_id).await;
    h.publish(plan_id, seeded.revision).await;
    h.publish_price(plan_id, seeded.price_id).await;

    let frozen = h
        .governance
        .synthesis
        .synthesize(
            &h.scope(),
            h.tenant,
            synthesis_request(Uuid::now_v7(), plan_id, covered_at()),
        )
        .await
        .expect("the snapshot freezes");

    assert_eq!(
        frozen.record.source_revision,
        Some(seeded.revision),
        "the provenance names the revision it was frozen from: {:?}",
        frozen.record
    );
}

// ---------------------------------------------------------------------------
// D-87's payload.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_frozen_payload_is_self_contained_and_says_it_has_no_catalog_version() {
    // D-87: a `migrated-origin` ref resolves through **no** `CatalogVersion`, so
    // rating and Billing can look nothing up and everything they need is here.
    let h = Harness::new().await;
    let plan_id = covered_plan(&h).await;
    let subscription = Uuid::now_v7();

    let frozen = h
        .governance
        .synthesis
        .synthesize(
            &h.scope(),
            h.tenant,
            synthesis_request(subscription, plan_id, covered_at()),
        )
        .await
        .expect("synthesize");

    let payload = &frozen.record.payload;
    // Stated rather than merely absent, so a consumer looking for a version
    // learns why there is none instead of failing.
    assert!(payload["catalogVersion"].is_null());
    assert_eq!(payload["catalogVersionDeliberatelyAbsent"], true);

    // The row half: evaluable content, not just ids.
    let rows = payload["rows"].as_array().expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["currency"], "EUR");
    assert_eq!(rows[0]["region"], "eu");
    assert_eq!(rows[0]["source"], "live_history");
    assert!(rows[0].get("modelKind").is_some());
    assert!(rows[0].get("taxInclusive").is_some());
    assert!(rows[0].get("roundingPolicyRef").is_some());

    // **The positive control for D-323.** This plan's row is `flat`, so its money
    // is in `amountMinor` and the rate member is NULL — the other half of the
    // placement matrix `a_per_unit_lines_rate_reaches_the_frozen_payload` reads.
    // Without a case that pins the amount arm, teaching this builder about rates
    // and rewriting it would look the same from here.
    assert_eq!(
        rows[0]["amountMinor"], 9_900,
        "a flat row still renders its amount unchanged: {}",
        frozen.record.payload
    );
    assert!(
        rows[0]["unitRateNanoMinor"].is_null(),
        "and carries no rate, because a flat row prices by no multiple: {}",
        frozen.record.payload
    );

    // **The positive controls for the band set and the S6 contract.** A `flat`
    // row authors no bands, so its ladder is the **empty** one and not an absent
    // member — the set is read on every row, so an empty array here means "this
    // row has no bands" and nothing else. Teaching this builder about ladders
    // and rewriting what it renders for an untiered row would look the same from
    // here without this line.
    assert_eq!(
        rows[0]["bands"],
        serde_json::json!([]),
        "a flat row's ladder is empty and read, not absent and unread: {}",
        frozen.record.payload
    );
    // The seed anchors on the calendar month, which carries no day; the
    // `fixed_day` arm of the same matrix is
    // `the_proration_contract_and_the_manual_quantity_reach_the_frozen_payload`.
    assert_eq!(rows[0]["billingAnchorPolicy"], "calendar_month");
    assert!(rows[0]["anchorDay"].is_null());
    assert_eq!(rows[0]["prorationBasis"], "calendar_days_actual");
    assert_eq!(rows[0]["creditOnDowngrade"], false);
    assert!(rows[0]["quantitySource"].is_null());
    assert!(rows[0]["manualQuantity"].is_null());

    // C-5's plan-level half, and the absence that is reported rather than hidden:
    // there is no entitlement grant store in this gear, so a consumer must not
    // read the empty set as "this plan grants nothing".
    assert_eq!(payload["planLevel"]["grantSetUnavailable"], true);
    assert!(payload["planLevel"]["grantSet"].is_null());
    assert!(payload["planLevel"].get("invoiceLineTemplate").is_some());
    // D-319, and the marker beside it is the point: this plan authored no
    // minimum, so the empty list **means** "no minimum" — which is only readable
    // because the payload also says the set was available to be read. Without
    // the marker an unread set and an unauthored one render identically, and
    // this payload resolves through no `CatalogVersion`, so a consumer has
    // nowhere to go and check.
    assert_eq!(
        payload["planLevel"]["periodFloorCaps"],
        serde_json::json!([])
    );
    assert_eq!(payload["planLevel"]["periodFloorCapsUnavailable"], false);
}

/// **A `per_unit` line's rate reaches the frozen payload, and it is the only
/// price it has** (D-323).
///
/// D-311 moved a `per_unit` row's money out of `amount_minor` and into
/// `unit_rate_nano`, and listed ten files as the propagation surface;
/// `infra::synthesis` was in none of them. The builder rendered `amountMinor`
/// alone, so every synthesized `per_unit` line reached Rating with
/// `"amountMinor": null` and no price anywhere in the payload.
///
/// **`amount_minor` is not merely absent here, it is forbidden.**
/// `check_amount_placement` refuses a `per_unit` row that carries one — *"two
/// priced columns are two competing prices"* — so there is no publishable row of
/// this kind for which the old rendering could have produced a number. The
/// assertion on `amountMinor` below is therefore not incidental: it is what says
/// the rate member is the row's whole price, and a case that only asserted the
/// rate would not have said it.
///
/// The record is INSERT-only and resolves through **no** `CatalogVersion` (D-87),
/// so this is not a value a later publish corrects.
#[tokio::test]
async fn a_per_unit_lines_rate_reaches_the_frozen_payload() {
    // €0.023 per unit, in D-311's nano-minor scale — the S3 rate the decision was
    // raised on, and one no amount column can express.
    const RATE_NANO_MINOR: i64 = 23_000_000;

    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_per_unit_plan(&h, plan_id, RATE_NANO_MINOR).await;
    h.publish(plan_id, seeded.revision).await;
    h.publish_price(plan_id, seeded.price_id).await;

    let frozen = h
        .governance
        .synthesis
        .synthesize(
            &h.scope(),
            h.tenant,
            synthesis_request(Uuid::now_v7(), plan_id, covered_at()),
        )
        .await
        .expect("synthesize");

    let rows = frozen.record.payload["rows"].as_array().expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["modelKind"], "per_unit");
    assert!(
        rows[0]["amountMinor"].is_null(),
        "the placement matrix forbids an amount on a per_unit row, so nothing else in this \
         payload can be carrying the price: {}",
        frozen.record.payload
    );
    assert_eq!(
        rows[0]["unitRateNanoMinor"], RATE_NANO_MINOR,
        "the rate is the line's only price and the payload is frozen: {}",
        frozen.record.payload
    );
    // The second positive control for the band set: `inst-mk-forbidden` refuses
    // bands on a `per_unit` row — they would be a second, unreachable price — so
    // the ladder here is empty for a reason the placement matrix states, and a
    // builder that rendered a ladder on this row would be inventing one.
    assert_eq!(
        rows[0]["bands"],
        serde_json::json!([]),
        "bands are forbidden on a per_unit row: {}",
        frozen.record.payload
    );
}

/// The ladder the tiered fixture prices on, in D-311's nano-minor scale.
///
/// Descending, so no band trips `TIER_BAND_PRICE_INCREASE`'s advisory, and none
/// of the three rates is a whole number of minor units: a payload that rendered
/// the band rate through any minor-unit path would have to round, and these
/// values make the rounding visible instead of plausible.
fn seeded_bands() -> Vec<TierBand> {
    let rate = |nano: i64| RateMinor::from_nano_minor(nano).expect("a non-negative rate");
    vec![
        TierBand::closed(0, 100, rate(40_500_000)),
        TierBand::closed(100, 1_000, rate(25_250_000)),
        TierBand::open(1_000, rate(10_125_000)),
    ]
}

/// **A tiered line's band set reaches the frozen payload, and it is the only
/// price it has.**
///
/// D-87 clause 1b names *"the ordered band set"* among the evaluable row content
/// this payload materializes, and `infra::synthesis` rendered none of it: a band
/// rate lives in `pricing_price_tier_band.unit_price_nano` and the builder read
/// only `pricing_price`. This is D-323's defect one class wider — that one lost
/// a `per_unit` row's rate, this one loses a `graduated` / `volume` row's whole
/// ladder.
///
/// **Both scalar money columns are forbidden here, not merely absent.**
/// `check_amount_placement` gives `graduated` `(wants_amount, wants_rate) =
/// (false, false)` and refuses either column present — *"two priced columns are
/// two competing prices"* — so there is no publishable row of this kind for
/// which the old rendering could have produced a number anywhere. The two
/// `is_null` assertions below are what say that: a case asserting only the bands
/// would not have said the ladder is the line's whole price.
///
/// The bands are asserted **as authored**, whole and in `from_qty` order. The
/// record is INSERT-only and resolves through no `CatalogVersion` (D-87), so
/// this is not a value a later publish corrects.
#[tokio::test]
async fn a_tiered_lines_band_set_reaches_the_frozen_payload() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_tiered_usage_plan(&h, plan_id, seeded_bands()).await;
    h.publish(plan_id, seeded.revision).await;
    h.publish_price(plan_id, seeded.price_id).await;

    let frozen = h
        .governance
        .synthesis
        .synthesize(
            &h.scope(),
            h.tenant,
            synthesis_request(Uuid::now_v7(), plan_id, covered_at()),
        )
        .await
        .expect("synthesize");

    let rows = frozen.record.payload["rows"].as_array().expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["modelKind"], "graduated");
    assert!(
        rows[0]["amountMinor"].is_null() && rows[0]["unitRateNanoMinor"].is_null(),
        "the placement matrix forbids both scalar money columns on a graduated row, so nothing \
         else in this payload can be carrying the price: {}",
        frozen.record.payload
    );
    assert_eq!(
        rows[0]["bands"],
        serde_json::json!([
            { "fromQty": 0, "toQty": 100, "unitPriceNanoMinor": 40_500_000_i64 },
            { "fromQty": 100, "toQty": 1_000, "unitPriceNanoMinor": 25_250_000_i64 },
            { "fromQty": 1_000, "toQty": null, "unitPriceNanoMinor": 10_125_000_i64 },
        ]),
        "the ladder is the line's only price and the payload is frozen: {}",
        frozen.record.payload
    );
}

/// **The Slice-10 content columns reach the frozen payload** — the reservation
/// pair, the typed floors, the discount hook and the level-aggregation hold.
///
/// Every one of them is a column read on every row of `pricing_price`, and every
/// one was absent from a record that resolves through **no** `CatalogVersion`
/// and is INSERT-only. `reservedRateNanoMinor` is a **rate** and not money
/// (D-311): `inst-rv-attrs` has Rating source the self-service reserved rate from
/// the row, and on a `migrated-origin` line there is no other row to source it
/// from, so it is carried at the rate's own 10⁻⁹ precision rather than rounded to
/// the currency's minor unit on the way out.
///
/// This paragraph and the assertion below both said `reservedRateMinor` until
/// 2026-08-18, and the assertion had been **failing on this branch** since the
/// emitter was renamed: `serde_json::Value` indexes a missing key to `Null`, so
/// the case reddened with `left: Null` rather than naming the rename. The fixture
/// had already been moved to the rate — it seeds `3_100_000_000_000` nano-minor,
/// which is the same 3,100 minor units the old assertion named — so what was
/// stale was this reading of it and nothing else.
///
/// The fixture authors all seven non-default, so a builder that dropped one
/// renders a `null` here rather than leaving a hole that stays a hole.
#[tokio::test]
async fn the_slice_10_content_columns_reach_the_frozen_payload() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_tiered_usage_plan(&h, plan_id, seeded_bands()).await;
    h.publish(plan_id, seeded.revision).await;
    h.publish_price(plan_id, seeded.price_id).await;

    let frozen = h
        .governance
        .synthesis
        .synthesize(
            &h.scope(),
            h.tenant,
            synthesis_request(Uuid::now_v7(), plan_id, covered_at()),
        )
        .await
        .expect("synthesize");

    let row = &frozen.record.payload["rows"][0];
    let payload = &frozen.record.payload;
    assert_eq!(
        row["reservedRateNanoMinor"], 3_100_000_000_000_i64,
        "{payload}"
    );
    assert_eq!(row["reservationFlavor"], "capacity", "{payload}");
    assert_eq!(row["minQtyPurchase"], 7, "{payload}");
    assert_eq!(row["minQtyUsage"], 11, "{payload}");
    assert_eq!(row["minQtyUsageFallback"], "exception", "{payload}");
    assert_eq!(row["discountRef"], "promo/spring", "{payload}");
    assert_eq!(row["maxHoldGranules"], 6, "{payload}");

    // **No allowance marker, and that is the decision rather than an omission.**
    // The marker is an artifact of the D-45 compile, and this payload renders the
    // row as authored — `includedAllowance` is the declaration, and a marker
    // beside it would be the first compiled artifact in a record that carries
    // none.
    assert!(
        row.get("allowanceMarker").is_none(),
        "the payload carries authored content only: {payload}"
    );

    // **One absence, one spelling.** `meter` and `dimensionKey` are the two
    // halves of the usage line, and the columns behind them differ in kind - one
    // is nullable, the other defaults to the empty string - so copying both raw
    // spelled the same absence two ways inside one frozen document. This plane
    // resolves through no `CatalogVersion` and is never rewritten, so a consumer
    // reading it has no second chance at the distinction.
    assert_eq!(
        row["dimensionKey"],
        serde_json::Value::Null,
        "an undimensioned line renders absent, as `meter` beside it does: {payload}"
    );
    // **But it no longer says so only in a comment** (review M-5). Rendering the
    // row as authored is the decision above; leaving a consumer unable to tell
    // this payload's shape from the read model's was not. A consumer reading
    // `modelKind: "per_unit"` with a rate and no bands bills the whole quantity
    // where the read model bills the quantity past the allowance — the included
    // GB, every period, out of a record that can never be corrected.
    assert_eq!(
        row["allowanceCompiled"], false,
        "the payload has to state that the D-45 compile is not applied: {payload}"
    );
}

/// **A usage line's billing timing is the projected constant, not the raw column**
/// (review M-4).
///
/// `published_billing_timing` is `usage → arrears`, `one_time → advance`, and only
/// `recurring` passes the column through. `inst-bt-required` requires the column
/// only on a `recurring` row and `check_setup_fields` forbids it on a setup row,
/// so on a usage line it is `NULL` in practice and by intent — and this door
/// rendered it raw until 2026-08-19, freezing `null` where the read model for the
/// identical row renders `arrears`.
///
/// **The usage line is the armed part.** A probe on a recurring row is green
/// against the defect, because that is the one kind whose column *is* the answer.
/// `published_billing_timing`'s own doc says why the column must not be handed
/// out: were an authored value allowed to displace the constant, Billing's
/// deferral on a usage line would depend on whether someone had typed into a
/// column the design says is not theirs to author.
#[tokio::test]
async fn a_usage_lines_billing_timing_is_projected_and_not_the_raw_column() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_tiered_usage_plan(&h, plan_id, seeded_bands()).await;
    h.publish(plan_id, seeded.revision).await;
    h.publish_price(plan_id, seeded.price_id).await;

    let frozen = h
        .governance
        .synthesis
        .synthesize(
            &h.scope(),
            h.tenant,
            synthesis_request(Uuid::now_v7(), plan_id, covered_at()),
        )
        .await
        .expect("synthesize");

    let row = &frozen.record.payload["rows"][0];
    let payload = &frozen.record.payload;
    assert_eq!(
        row["chargeKind"], "usage",
        "the fixture is the armed one: {payload}"
    );
    assert_eq!(
        row["billingTiming"], "arrears",
        "a usage line defers by rule, and the frozen record is evaluated from this and nothing \
         else: {payload}"
    );
}

/// **The S6 proration contract and the manual quantity reach the frozen
/// payload.**
///
/// The builder's own comment at the site said *"the evaluation-policy and S6
/// consumer-contract fields: a `migrated-origin` line is evaluated from this and
/// nothing else"*, and of the S6 set only `billingTiming` had made it. The other
/// four are what Subscriptions prorates from, and `inst-pi-credit-source` says
/// in as many words that on a plan change the governing `creditOnDowngrade` is
/// the source row's, *read from the subscriber's frozen snapshot* — which is
/// this record.
///
/// `manualQuantity` is the other half of a `manual` row's arithmetic: the
/// payload stated **where** the quantity comes from and never **what** it is,
/// while `check_quantity_source` refuses to publish the row without it.
#[tokio::test]
async fn the_proration_contract_and_the_manual_quantity_reach_the_frozen_payload() {
    const RATE_NANO_MINOR: i64 = 23_000_000;
    const MANUAL_QUANTITY: u64 = 12;
    const ANCHOR_DAY: u8 = 17;

    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_manual_quantity_plan(
        &h,
        plan_id,
        RATE_NANO_MINOR,
        MANUAL_QUANTITY,
        ANCHOR_DAY,
    )
    .await;
    h.publish(plan_id, seeded.revision).await;
    h.publish_price(plan_id, seeded.price_id).await;

    let frozen = h
        .governance
        .synthesis
        .synthesize(
            &h.scope(),
            h.tenant,
            synthesis_request(Uuid::now_v7(), plan_id, covered_at()),
        )
        .await
        .expect("synthesize");

    let row = &frozen.record.payload["rows"][0];
    let payload = &frozen.record.payload;
    assert_eq!(row["quantitySource"], "manual", "{payload}");
    assert_eq!(
        row["manualQuantity"], 12,
        "a manual row's quantity is half its arithmetic and the rate is the other half: {payload}"
    );
    assert_eq!(row["billingAnchorPolicy"], "fixed_day", "{payload}");
    assert_eq!(
        row["anchorDay"], 17,
        "the day is a second fact beside the policy token, and only fixed_day carries \
         one: {payload}"
    );
    assert_eq!(row["prorationBasis"], "calendar_days_actual", "{payload}");
    assert_eq!(
        row["creditOnDowngrade"], true,
        "authored true here against the flat seed's false, so the member is pinned to a value \
         rather than to a default: {payload}"
    );
}

/// **A period bound reaches the frozen `migrated-origin` payload** (D-319).
///
/// The case exists because this is the gear's **second** plan-level payload and
/// it does not go through `publish::assemble_from`: `plan_level` reads the
/// descriptor set off the current revision by hand, and a field added to the
/// read model alone would be invisible here. That matters more here than
/// anywhere else — a `migrated-origin` ref resolves through **no**
/// `CatalogVersion` by construction, so a bound outside this payload is one
/// Billing cannot apply and cannot look up, on a record that is frozen and
/// therefore permanently wrong.
#[tokio::test]
async fn a_period_bound_is_materialized_into_the_frozen_payload() {
    let h = Harness::new().await;
    let plan_id = Uuid::now_v7();
    let seeded = seed_publishable_plan(&h, plan_id).await;

    // On the market the seeded row prices — anything else is
    // `PERIOD_FLOOR_CAP_MARKET_UNSOLD` and would never publish.
    h.state
        .shapes
        .replace_period_floor_caps(
            &h.scope(),
            h.tenant,
            PlanId::new(plan_id),
            seeded.revision,
            seeded.version,
            vec![PeriodFloorCap {
                currency: CurrencyCode::new("EUR").expect("three letters"),
                region: Region::new("eu").expect("non-blank"),
                floor_minor: Some(MinorAmount::new(50_000).expect("non-negative")),
                cap_minor: None,
            }],
            seed_stamp(),
        )
        .await
        .expect("author the period floor on the open draft");

    h.publish(plan_id, seeded.revision).await;
    h.publish_price(plan_id, seeded.price_id).await;

    let frozen = h
        .governance
        .synthesis
        .synthesize(
            &h.scope(),
            h.tenant,
            synthesis_request(Uuid::now_v7(), plan_id, covered_at()),
        )
        .await
        .expect("synthesize");

    assert_eq!(
        frozen.record.payload["planLevel"]["periodFloorCaps"],
        serde_json::json!([{
            "currency": "EUR",
            "region": "eu",
            "floorMinor": 50_000,
            "capMinor": null,
        }]),
        "the bound is materialized whole: {}",
        frozen.record.payload
    );
    assert_eq!(
        frozen.record.payload["planLevel"]["periodFloorCapsUnavailable"],
        false
    );
}

// ---------------------------------------------------------------------------
// D-102's read surface.
// ---------------------------------------------------------------------------

/// **The route serves the payload the service stored, member for member.**
///
/// The coverage inversion this file is, closed at one assertion. Nearly every
/// case here calls `h.governance.synthesis.synthesize(...)` directly and asserts
/// `frozen.record.payload` — the value the *service* stored, not the value the
/// *route* serves — so every "the frozen payload carries X" claim in this file
/// would pass against a read handler that filtered, renamed or projected a member
/// on the way out. That is a real risk on this surface and not a theoretical one:
/// the payload is a free-form `serde_json::Value` with no DTO between the store and
/// the wire, so nothing types the shape a consumer receives.
///
/// Comparing the whole document is what makes this one case stand in for all
/// fourteen: any projection, at any depth, in any member, fails here.
#[tokio::test]
async fn the_route_serves_the_stored_payload_unprojected() {
    let h = Harness::new().await;
    let plan_id = covered_plan(&h).await;
    let subscription = Uuid::now_v7();
    let frozen = h
        .governance
        .synthesis
        .synthesize(
            &h.scope(),
            h.tenant,
            synthesis_request(subscription, plan_id, covered_at()),
        )
        .await
        .expect("synthesize");

    let response = h
        .allowed_as(RATING_SERVICE)
        .send(request("GET", &path(subscription), None))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let view = body_json(response).await;

    assert_eq!(
        view["payload"], frozen.record.payload,
        "the wire payload is the stored payload; a handler that projected any member \
         of it would leave every `frozen.record.payload` assertion in this file green"
    );
    assert_eq!(
        view["resolved"], frozen.record.resolved,
        "and the provenance travels with it, which is what `inst-sy-provenance` is"
    );

    // The anti-tautology: the payload has to be a document with members in it, or
    // an equality between two empty values would satisfy the assertions above.
    assert!(
        view["payload"]["rows"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty()),
        "the fixture froze at least one row: {view}"
    );
}

#[tokio::test]
async fn the_surface_returns_the_frozen_payload_with_its_provenance() {
    let h = Harness::new().await;
    let plan_id = covered_plan(&h).await;
    let subscription = Uuid::now_v7();
    h.governance
        .synthesis
        .synthesize(
            &h.scope(),
            h.tenant,
            synthesis_request(subscription, plan_id, covered_at()),
        )
        .await
        .expect("synthesize");

    let response = h
        .allowed_as(RATING_SERVICE)
        .send(request("GET", &path(subscription), None))
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    let view = body_json(response).await;
    assert_eq!(view["subscription_ref"], subscription.to_string());
    assert_eq!(view["trigger"], "migration");
    assert_eq!(view["payload"]["catalogVersionDeliberatelyAbsent"], true);
    assert_eq!(view["resolved"][0]["source"], "live_history");
}

#[tokio::test]
async fn the_surface_answers_404_before_synthesis_and_that_is_the_contract() {
    // `inst-sy-firstrating`: rating a legacy subscription before synthesis fails
    // closed into the exception path, synthesis runs as a separate audited step,
    // and rating retries. A 200 carrying a partial payload would make those two
    // states indistinguishable.
    let h = Harness::new().await;
    let response = h
        .allowed_as(RATING_SERVICE)
        .send(request("GET", &path(Uuid::now_v7()), None))
        .await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    // The family, not only the status. This suite carried no discriminator on
    // either refusal, so the 404 that says "synthesis has not run" and any other
    // 404 in the stack read identically — which is the pair `inst-sy-firstrating`
    // needs told apart.
    assert_eq!(problem_family(response).await, "not_found");
}

#[tokio::test]
async fn a_caller_without_plan_read_is_denied() {
    let h = Harness::new().await;
    let plan_id = covered_plan(&h).await;
    let subscription = Uuid::now_v7();
    h.governance
        .synthesis
        .synthesize(
            &h.scope(),
            h.tenant,
            synthesis_request(subscription, plan_id, covered_at()),
        )
        .await
        .expect("synthesize");

    let response = h
        .denied()
        .send(request("GET", &path(subscription), None))
        .await;

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(problem_family(response).await, "permission_denied");
}
