//! Phase 1's store-dependent half, executed against a real store
//! (`design/12-operator-efficiency.md` §3 `inst-bk-phase1`; D-118).
//!
//! **Driven through the price repository rather than over a hand-built map**,
//! for `tests/sqlite_clone.rs`'s reason and for one this suite learned the hard
//! way: the rule mirrors what the store holds, and a fixture the test assembled
//! itself would prove only that the test and the rule agree. D-283 is exactly
//! that failure — a duplicate rule judged against an index nobody read, green
//! beside a sibling suite asserting the opposite through the store.
//!
//! What the batch-only half asserts is in `domain::import`'s own tests; nothing
//! here repeats it.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use bss_pricing::domain::approval::PENDING_CHANGE_UNIT_EXISTS;
use bss_pricing::domain::audit::{AuditStamp, AuditSubjectKind};
use bss_pricing::domain::import::{
    BatchReport, IMPORT_TARGETS_PUBLISHED, ImportRow, RowViolation, classify,
};
use bss_pricing::domain::instant::utc_ymd_hms;
use bss_pricing::domain::money::{CurrencyCode, MinorAmount};
use bss_pricing::domain::price_record::PriceContent;
use bss_pricing::domain::price_row::{IncludedAllowance, ModelKind, PriceRow, RolloverPolicy};
use bss_pricing::domain::publish::rules::PRIMITIVE_RULES_UNBUILT;
use bss_pricing::domain::scope_key::{
    ChargeKind, Cohort, DimensionKey, Meter, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use bss_pricing::infra::import::classify_against_store;
use bss_pricing::infra::storage::migrations::Migrator;
use bss_pricing::infra::storage::repo::{NewApproval, NewPriceDraft, PriceRepo, approval_repo};
use time::OffsetDateTime;

use sea_orm_migration::MigratorTrait;
use std::collections::BTreeSet;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::AccessScope;
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use uuid::Uuid;

const TENANT: Uuid = Uuid::from_u128(0x7e_31);
const ACTOR: Uuid = Uuid::from_u128(0xac_30);
const CORRELATION: Uuid = Uuid::from_u128(0xc0_31);
/// A second tenant, so the two store-backed refusals can be asked whether they are
/// about the key or about the row.
const OTHER_TENANT: Uuid = Uuid::from_u128(0x7e_42);

fn plan() -> PlanId {
    PlanId::new(Uuid::from_u128(0x50_c3))
}
fn phase() -> PhaseId {
    PhaseId::new(Uuid::from_u128(0xfa_70))
}
fn at(hour: u32) -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 9, hour, 0, 0)
}
fn scope() -> AccessScope {
    AccessScope::for_tenant(TENANT)
}

fn key(region: &str) -> ScopeKey {
    ScopeKey::new(
        plan(),
        CurrencyCode::new("EUR").expect("three letters"),
        Region::new(region).expect("a non-blank region"),
        phase(),
        PriceEligibility::AllSubscriptions,
        ChargeKind::Recurring,
        Cohort::None,
    )
    .expect("the class pairs with the cohort")
}

fn content(amount: i64) -> PriceContent {
    let mut row = PriceRow::new(ChargeKind::Recurring, Some(ModelKind::Flat));
    row.amount_minor = Some(MinorAmount::new(amount).expect("a non-negative amount"));
    PriceContent {
        row,
        tax_inclusive: false,
        tax_category_ref: None,
        billing_timing: Some("advance".to_owned()),
        proration_contract: None,
        rounding_policy_ref: Some("half_up".to_owned()),
        grandfather_until: None,
        supersedes_price_id: None,
    }
}

fn row(scope_key: ScopeKey, amount: i64) -> ImportRow {
    ImportRow {
        scope_key,
        content: content(amount),
        if_match: None,
    }
}

struct Harness {
    provider: DBProvider<DbError>,
    prices: PriceRepo,
}

async fn harness() -> Harness {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect in-memory sqlite");
    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("run migrator");
    let provider = DBProvider::<DbError>::new(db);
    let prices = PriceRepo::new(provider.clone());
    Harness { provider, prices }
}

/// Author a row and publish it past the engine, so the key is genuinely held.
async fn publish(h: &Harness, scope_key: ScopeKey, amount: i64) -> Uuid {
    publish_for(h, TENANT, scope_key, amount).await
}

/// [`publish`] under a named tenant — the operand the cross-tenant case varies,
/// and the only one it varies.
async fn publish_for(h: &Harness, tenant: Uuid, scope_key: ScopeKey, amount: i64) -> Uuid {
    let scope = AccessScope::for_tenant(tenant);
    let price_id = Uuid::now_v7();
    h.prices
        .create_draft(
            &scope,
            tenant,
            NewPriceDraft {
                price_id,
                scope_key,
                content: content(amount),
                created_by: ACTOR,
                created_at_utc: at(10),
                correlation_id: CORRELATION,
            },
        )
        .await
        .expect("author the row");
    common::publish_row_directly(&h.provider, &scope, price_id).await;
    price_id
}

/// A usage row's content, whose own line must agree with its key — the store
/// refuses `UsageLineDisagrees` otherwise, which is the guard that makes a
/// mismatched `ImportRow` unauthorable rather than merely wrong.
/// A usage row's content, and its kind is `per_unit` — the plain untiered metered
/// rate, unit price times metered `Q`.
///
/// It was `flat` until D-312's bulk arm was built, and `flat` is in no part of the
/// usage set: this fixture had been carrying a key contradiction, so
/// `a_usage_line_key_is_matched_through_the_store_like_any_other` began hearing
/// `MODEL_KIND_CHARGEKIND_MISMATCH` beside the `IMPORT_TARGETS_PUBLISHED` it asserts.
/// That report was right — a row hears every violation found against it — and the
/// fixture was wrong.
fn usage_content(meter: &str, amount: i64) -> PriceContent {
    let mut row = PriceRow::new(ChargeKind::Usage, Some(ModelKind::PerUnit));
    row.amount_minor = Some(MinorAmount::new(amount).expect("a non-negative amount"));
    row.meter = Some(meter.to_owned());
    "region=eu".clone_into(&mut row.dimension_key);
    PriceContent {
        row,
        tax_inclusive: false,
        tax_category_ref: None,
        billing_timing: Some("arrears".to_owned()),
        proration_contract: None,
        rounding_policy_ref: Some("half_up".to_owned()),
        grandfather_until: None,
        supersedes_price_id: None,
    }
}

fn usage_row(scope_key: ScopeKey, meter: &str, amount: i64) -> ImportRow {
    ImportRow {
        scope_key,
        content: usage_content(meter, amount),
        if_match: None,
    }
}

/// The metered `(eu, api-calls, region=eu)` line — the key a Slice-10 primitive
/// is authored on.
///
/// The two ordering cases below hung an `includedAllowance` on a `flat`
/// **recurring** row, which was invisible while nothing judged the field and
/// became a second Phase-1 refusal the moment `inst-ac-gate` landed
/// (`ALLOWANCE_ON_NON_USAGE`, write-stage). Both cases assert an exact code
/// vector, so both would have gone red on a fault that is the fixture's rather
/// than the report's. The fixture is fixed, exactly as `usage_content` was one
/// wave earlier for the same class of fault.
fn metered_key() -> ScopeKey {
    ScopeKey::new(
        plan(),
        CurrencyCode::new("EUR").expect("three letters"),
        Region::new("eu").expect("a non-blank region"),
        phase(),
        PriceEligibility::AllSubscriptions,
        ChargeKind::Usage,
        Cohort::None,
    )
    .expect("the class pairs with the cohort")
    .with_usage_line(
        Some(Meter::new("api-calls").expect("a meter")),
        DimensionKey::new("region=eu"),
    )
    .expect("a usage line on a usage key")
}

async fn publish_usage(h: &Harness, scope_key: ScopeKey, meter: &str, amount: i64) -> Uuid {
    let price_id = Uuid::now_v7();
    h.prices
        .create_draft(
            &scope(),
            TENANT,
            NewPriceDraft {
                price_id,
                scope_key,
                content: usage_content(meter, amount),
                created_by: ACTOR,
                created_at_utc: at(10),
                correlation_id: CORRELATION,
            },
        )
        .await
        .expect("author the usage row");
    common::publish_row_directly(&h.provider, &scope(), price_id).await;
    price_id
}

async fn judged(h: &Harness, rows: &[ImportRow]) -> BatchReport {
    let mut report = classify(rows);
    let conn = h.provider.conn().expect("conn");
    classify_against_store(&conn, &scope(), TENANT, rows, &mut report)
        .await
        .expect("the store answers");
    report
}

fn codes(report: &BatchReport, row: usize) -> Vec<String> {
    report
        .rows()
        .iter()
        .find(|outcome| outcome.row == row)
        .map(|outcome| {
            outcome
                .violations
                .iter()
                .map(|violation: &RowViolation| violation.code.clone())
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn a_row_changing_a_published_rows_content_is_refused_and_named() {
    let h = harness().await;
    let held = publish(&h, key("eu"), 9_900).await;

    let report = judged(&h, &[row(key("eu"), 12_500)]).await;

    assert_eq!(codes(&report, 0), vec![IMPORT_TARGETS_PUBLISHED]);
    let detail = &report.rows()[0].violations[0].detail;
    assert!(
        detail.contains(&held.to_string()),
        "the refusal names the row that holds the key: {detail}"
    );
    assert!(
        detail.contains("repricing run"),
        "and the remedy, which exists: {detail}"
    );
    assert!(report.blocks_the_batch());
}

#[tokio::test]
async fn a_row_identical_to_the_published_one_is_refused_too() {
    // **The carve-out that was, and is not** (D-287). `inst-bk-phase1` says "with
    // changed content", and this refuses whatever the content is — because an
    // unchanged row authors a no-op draft beside the published row, and the
    // supersession door refuses a draft occupant, so re-running an unchanged
    // batch would block the repricing run this very message names as the remedy.
    // The re-imported file is served by `inst-bk-idem`'s replay, not here.
    let h = harness().await;
    publish(&h, key("eu"), 9_900).await;

    let report = judged(&h, &[row(key("eu"), 9_900)]).await;

    assert_eq!(codes(&report, 0), vec![IMPORT_TARGETS_PUBLISHED]);
    assert!(report.blocks_the_batch());
}

#[tokio::test]
async fn a_usage_line_key_is_matched_through_the_store_like_any_other() {
    // **The axis pair D-283 was about, checked through the store** — the suite
    // created for that lesson did not exercise it. A published usage row holds
    // its key; a row on a *different* meter is a different key and is untouched.
    let h = harness().await;
    let usage = ScopeKey::new(
        plan(),
        CurrencyCode::new("EUR").expect("three letters"),
        Region::new("eu").expect("a non-blank region"),
        phase(),
        PriceEligibility::AllSubscriptions,
        ChargeKind::Usage,
        Cohort::None,
    )
    .expect("the class pairs with the cohort");
    let metered = usage
        .clone()
        .with_usage_line(
            Some(Meter::new("api-calls").expect("a meter")),
            DimensionKey::new("region=eu"),
        )
        .expect("a usage line on a usage key");
    let other = usage
        .with_usage_line(
            Some(Meter::new("storage-gb").expect("a meter")),
            DimensionKey::new("region=eu"),
        )
        .expect("a usage line on a usage key");
    publish_usage(&h, metered.clone(), "api-calls", 900).await;

    let report = judged(
        &h,
        &[
            usage_row(metered, "api-calls", 1_200),
            usage_row(other, "storage-gb", 1_200),
        ],
    )
    .await;

    assert_eq!(codes(&report, 0), vec![IMPORT_TARGETS_PUBLISHED]);
    assert!(
        codes(&report, 1).is_empty(),
        "a second meter is a second key and holds nothing (D-196, D-283)"
    );
}

#[tokio::test]
async fn a_row_that_is_both_unjudged_and_published_hears_them_in_a_stable_order() {
    // **The ordering the report claims and nothing pinned.** The two codes are
    // inserted `[PRIMITIVE_RULES_UNBUILT, IMPORT_TARGETS_PUBLISHED]` — the
    // batch-only half runs first — and sort to the reverse. Delete the sort in
    // `BatchReport::add` and this is the case that reddens.
    let h = harness().await;
    publish_usage(&h, metered_key(), "api-calls", 900).await;
    let mut carrier = usage_row(metered_key(), "api-calls", 1_200);
    carrier.content.row.included_allowance = Some(IncludedAllowance {
        quantity: 100,
        rollover_policy: RolloverPolicy::Carry,
    });

    let report = judged(&h, &[carrier]).await;

    assert_eq!(
        codes(&report, 0),
        vec![IMPORT_TARGETS_PUBLISHED, PRIMITIVE_RULES_UNBUILT],
        "sorted by code, not by the order the halves ran in"
    );
}

#[tokio::test]
async fn a_row_on_a_free_key_is_untouched_by_the_store_half() {
    let h = harness().await;
    publish(&h, key("eu"), 9_900).await;

    let report = judged(&h, &[row(key("us"), 12_500)]).await;

    assert!(
        report.rows().is_empty(),
        "no published row holds `us`, so nothing here refuses it: {report:?}"
    );
}

#[tokio::test]
async fn a_draft_row_on_the_key_is_not_a_published_row() {
    // The rule is about the **published** plane. A draft occupant is what an
    // import edits under its version — Phase 2's business, not this one's.
    let h = harness().await;
    h.prices
        .create_draft(
            &scope(),
            TENANT,
            NewPriceDraft {
                price_id: Uuid::now_v7(),
                scope_key: key("eu"),
                content: content(9_900),
                created_by: ACTOR,
                created_at_utc: at(10),
                correlation_id: CORRELATION,
            },
        )
        .await
        .expect("author a draft and leave it a draft");

    let report = judged(&h, &[row(key("eu"), 12_500)]).await;

    assert!(
        report.rows().is_empty(),
        "a draft occupant is not a published one: {report:?}"
    );
}

/// Open a `submitted` approval unit holding the given keys, so they are genuinely
/// reserved by the one-pending-unit rule.
async fn hold(h: &Harness, keys: &[ScopeKey]) -> Uuid {
    hold_for(h, TENANT, keys).await
}

/// [`hold`] under a named tenant.
async fn hold_for(h: &Harness, tenant: Uuid, keys: &[ScopeKey]) -> Uuid {
    let approval_id = Uuid::now_v7();
    let conn = h.provider.conn().expect("conn");
    approval_repo::open(
        &conn,
        &AccessScope::for_tenant(tenant),
        NewApproval {
            approval_id,
            tenant_id: tenant,
            subject_ref: format!("{}/0", plan()),
            subject_kind: AuditSubjectKind::PlanRevision,
            content_hash: vec![0x11; 32],
            materiality: serde_json::json!({ "material": true }),
            held_keys: keys
                .iter()
                .map(ToString::to_string)
                .collect::<BTreeSet<_>>(),
        },
        AuditStamp {
            actor_principal_id: ACTOR,
            recorded_at: at(10),
            correlation_id: CORRELATION,
        },
    )
    .await
    .expect("open the pending unit");
    approval_id
}

#[tokio::test]
async fn a_row_whose_key_a_pending_unit_holds_is_refused_and_names_it() {
    let h = harness().await;
    let unit = hold(&h, &[key("eu")]).await;

    let report = judged(&h, &[row(key("eu"), 12_500)]).await;

    assert_eq!(codes(&report, 0), vec![PENDING_CHANGE_UNIT_EXISTS]);
    let detail = &report.rows()[0].violations[0].detail;
    assert!(
        detail.contains(&unit.to_string()),
        "the operator has to know which unit to decide or withdraw: {detail}"
    );
}

#[tokio::test]
async fn one_unit_holding_two_of_the_batchs_keys_refuses_both_rows() {
    // **The reason the register read needed a plural.** A submit stops at the
    // first conflict because one is enough to refuse; Phase 1 owes a verdict on
    // every row, and a caller that stopped at the first would send the operator
    // round once per held key.
    let h = harness().await;
    hold(&h, &[key("eu"), key("us")]).await;

    let report = judged(&h, &[row(key("eu"), 12_500), row(key("us"), 7_500)]).await;

    assert_eq!(codes(&report, 0), vec![PENDING_CHANGE_UNIT_EXISTS]);
    assert_eq!(codes(&report, 1), vec![PENDING_CHANGE_UNIT_EXISTS]);
}

#[tokio::test]
async fn a_held_key_does_not_taint_its_neighbours() {
    let h = harness().await;
    hold(&h, &[key("eu")]).await;

    let report = judged(&h, &[row(key("eu"), 12_500), row(key("us"), 7_500)]).await;

    assert_eq!(codes(&report, 0), vec![PENDING_CHANGE_UNIT_EXISTS]);
    assert!(
        codes(&report, 1).is_empty(),
        "row 1's key is free and must be reported clean"
    );
}

#[tokio::test]
async fn the_report_is_in_batch_order_however_the_halves_reached_it() {
    // **The other half of the ordering claim, and it was pinned by nothing**
    // (D-289). Every earlier case adds rows in ascending index order, so
    // `binary_search_by_key` + `insert(at, …)` could be replaced by `push` and
    // the suite would stay green. Here the batch-only half fails **row 1** (a
    // Slice-10 primitive) and the store half fails **row 0** (a published key),
    // so `add` is called out of order and the report has to sort it out.
    let h = harness().await;
    publish(&h, key("eu"), 9_900).await;
    let mut carrier = usage_row(metered_key(), "api-calls", 1_200);
    carrier.content.row.included_allowance = Some(IncludedAllowance {
        quantity: 100,
        rollover_policy: RolloverPolicy::Carry,
    });

    let report = judged(&h, &[row(key("eu"), 12_500), carrier]).await;

    assert_eq!(
        report
            .rows()
            .iter()
            .map(|outcome| outcome.row)
            .collect::<Vec<_>>(),
        vec![0, 1],
        "in batch order, not in the order the halves happened to write"
    );
    assert_eq!(codes(&report, 0), vec![IMPORT_TARGETS_PUBLISHED]);
    assert_eq!(codes(&report, 1), vec![PRIMITIVE_RULES_UNBUILT]);
}

#[tokio::test]
async fn the_two_halves_write_one_report_and_a_row_hears_both() {
    // The merge API's whole point: a row can be a duplicate *and* aimed at a
    // published key, and the operator has to be told both or they fix one and
    // resubmit into the other.
    let h = harness().await;
    publish(&h, key("eu"), 9_900).await;

    let report = judged(&h, &[row(key("eu"), 12_500), row(key("eu"), 12_500)]).await;

    assert_eq!(
        report.rows().len(),
        2,
        "one entry per row, not one per rule"
    );
    for index in [0, 1] {
        assert_eq!(
            codes(&report, index),
            vec!["DUPLICATE_SCOPE_KEY", IMPORT_TARGETS_PUBLISHED],
            "row {index} is both, in a stable order"
        );
    }
}

#[tokio::test]
async fn one_read_serves_every_row_of_a_plan_and_a_batch_may_span_plans() {
    // Two plans in one batch, each with a held key. The point is not the read
    // count — nothing here can see it — but that a batch spanning plans is
    // judged correctly at all, which a per-plan read written for one plan would
    // get wrong.
    let h = harness().await;
    let other = PlanId::new(Uuid::from_u128(0x50_c4));
    let first_holder = publish(&h, key("eu"), 9_900).await;
    let there = ScopeKey::new(
        other,
        CurrencyCode::new("EUR").expect("three letters"),
        Region::new("eu").expect("a non-blank region"),
        phase(),
        PriceEligibility::AllSubscriptions,
        ChargeKind::Recurring,
        Cohort::None,
    )
    .expect("the class pairs with the cohort");
    let second_holder = publish(&h, there.clone(), 5_000).await;

    let report = judged(&h, &[row(key("eu"), 12_500), row(there, 7_500)]).await;

    assert_eq!(codes(&report, 0), vec![IMPORT_TARGETS_PUBLISHED]);
    assert_eq!(codes(&report, 1), vec![IMPORT_TARGETS_PUBLISHED]);
    assert!(
        report.rows()[0].violations[0]
            .detail
            .contains(&first_holder.to_string())
    );
    assert!(
        report.rows()[1].violations[0]
            .detail
            .contains(&second_holder.to_string())
    );
}

/// **Neither store-backed refusal is about another tenant's rows**, which nothing
/// here asked.
///
/// Both refusals this suite pins put a second row's identity on the wire — the
/// published `price_id` in `IMPORT_TARGETS_PUBLISHED`'s detail, the `approval_id` in
/// `PENDING_CHANGE_UNIT_EXISTS`'s — and both are read off the store by scope key.
/// A read that resolved the key without narrowing on the tenant would refuse this
/// batch and name a row and a unit the importer has no right to know exist: not a
/// leak of a price, but of the fact that another tenant sells on that key and of
/// the id an operator would then decide or withdraw.
///
/// The whole world is the other tenant's — a published row on the key *and* a
/// pending unit holding it — so a narrowing missing from either half is a red case
/// rather than a half-covered one. The positive control is the sibling pair
/// (`a_row_changing_a_published_rows_content_is_refused_and_named`,
/// `a_row_whose_key_a_pending_unit_holds_is_refused_and_names_it`): the identical
/// batch against the identical world under **this** tenant is refused by both
/// codes, so a clean report here is narrowing and not a rule that never runs. Both
/// halves are re-driven below, on this world, for exactly that reason.
#[tokio::test]
async fn another_tenants_published_row_and_pending_unit_leave_this_batch_clean() {
    let h = harness().await;
    let held = key("eu");
    publish_for(&h, OTHER_TENANT, held.clone(), 9_900).await;
    hold_for(&h, OTHER_TENANT, std::slice::from_ref(&held)).await;

    let report = judged(&h, &[row(held.clone(), 12_500)]).await;

    assert!(
        codes(&report, 0).is_empty(),
        "the key is free in this tenant; the report names {:?}",
        codes(&report, 0)
    );

    // The control, on this world rather than by reference to another case: the same
    // batch, the same key, this tenant's row and unit. Without it a
    // `classify_against_store` that had stopped reading the store at all would
    // satisfy the assertion above.
    publish_for(&h, TENANT, held.clone(), 9_900).await;
    let unit = hold_for(&h, TENANT, std::slice::from_ref(&held)).await;

    let refused = judged(&h, &[row(held, 12_500)]).await;
    assert_eq!(
        codes(&refused, 0),
        vec![
            IMPORT_TARGETS_PUBLISHED.to_owned(),
            PENDING_CHANGE_UNIT_EXISTS.to_owned()
        ],
        "both store-backed halves must answer for this tenant's own row, or the clean report \
         above is a rule that never ran"
    );
    let details: Vec<&str> = refused.rows()[0]
        .violations
        .iter()
        .map(|violation| violation.detail.as_str())
        .collect();
    assert!(
        details
            .iter()
            .any(|detail| detail.contains(&unit.to_string())),
        "and the unit it names is this tenant's rather than the other tenant's: {details:?}"
    );
}
