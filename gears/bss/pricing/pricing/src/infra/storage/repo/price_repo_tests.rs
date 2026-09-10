//! The two raw scope-key comparators, held to the key they claim to compare
//! (Z2-1, 2026-08-17).
//!
//! # Why this file is a test and not a refactor
//!
//! [`market_columns`] and [`scope_key_columns`] compare stored **columns** rather
//! than parsed [`ScopeKey`]s, and `scope_key_columns`' own doc gives the reason:
//! *"a comparison that had to parse first would answer 'corrupt' where the honest
//! answer is 'these two rows are not on one key'"*. That argument is sound and
//! this file does not touch it. What it supplies is the cover the argument costs.
//!
//! # What the cover has to be, and why the obvious one is not enough
//!
//! `domain::scope_key`'s register of ungated sites names three that build a key
//! **from** a row and says their partial cover is `ScopeKey::new`'s positional
//! signature — an axis added as a constructor parameter breaks them. There is a
//! fourth, and `market_columns` had **no** cover at all: it touches no
//! constructor, it is a bare eight-element tuple literal, and neither kind of
//! widening reaches it. It is also the worst place for that to be true:
//!
//! * It is deliberately partial — "all of them but `priceEligibility` and
//!   `cohort`" — so an eleventh axis silently absent from it does not read as
//!   "eight of ten". It reads as "modulo three axes", which is the shape the
//!   function is *supposed* to have.
//! * It is the row plane's last guard before `publish_rows`:
//!   [`refuse_ungenerational`] decides with it whether a cutover's grandfathered
//!   copy is a generation of the predecessor's market, and a comparison short by
//!   an axis admits a copy that differs on that axis onto a key nobody composed.
//! * It sits ten lines from `scope_key_columns`, whose doc is this crate's own
//!   account of exactly this defect shipping once: eight columns against a
//!   ten-axis key, from D-196 until 2026-08-06, which made `refuse_mispaired`
//!   read a successor on a **different meter of the same market** as being on the
//!   predecessor's key. The sweep that remediated that wrote the register of
//!   ungated sites and walked past the function immediately above the one it was
//!   documenting.
//!
//! So the cover here is not "these two tuples have ten and eight elements" — a
//! count is what a stale-count grep already fails at. It is one case **per axis**,
//! driven from an exhaustive [`ScopeKeyParts`] destructure, so an eleventh axis
//! makes this file stop compiling in the same commit that adds it.

use super::{check_update_keeps_the_line, market_columns, scope_key_columns, to_scope_key};
use crate::domain::instant::from_unix;
use crate::domain::instant::utc_ymd_hms;
use crate::domain::money::CurrencyCode;
use crate::domain::scope_key::{
    ChargeKind, Cohort, DimensionKey, Meter, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
    ScopeKeyParts,
};
use crate::infra::storage::RepoError;
use crate::infra::storage::entity::price;

const PLAN: uuid::Uuid = uuid::Uuid::from_u128(0x_91_a1);
const OTHER_PLAN: uuid::Uuid = uuid::Uuid::from_u128(0x_91_a2);
const PHASE: uuid::Uuid = uuid::Uuid::from_u128(0x_fa_a1);
const OTHER_PHASE: uuid::Uuid = uuid::Uuid::from_u128(0x_fa_a2);
const ACTOR: uuid::Uuid = uuid::Uuid::from_u128(0x_ac_a1);

/// A usage key on every axis this crate has, so **no** axis is at its default.
///
/// A base row whose `meter` were `None` or whose `cohort` were `none` would make
/// the per-axis edit below the only value that axis ever takes in this file, and
/// a comparator that ignored the column would still disagree — for the wrong
/// reason. Every axis moves *between two real values*.
fn base_key() -> ScopeKey {
    ScopeKey::new(
        PlanId::new(PLAN),
        CurrencyCode::new("EUR").expect("three letters"),
        Region::new("eu").expect("a non-blank region"),
        PhaseId::new(PHASE),
        PriceEligibility::ExistingGrandfathered,
        ChargeKind::Usage,
        Cohort::Generation(utc_ymd_hms(2099, 8, 20, 0, 0, 0)),
    )
    .expect("the grandfathered class pairs with a generation cohort")
    .with_usage_line(
        Some(Meter::new("api_calls").expect("a non-blank meter")),
        DimensionKey::new("region=eu"),
    )
    .expect("a usage line names its meter")
}

/// The stored row of `key`, with every non-key column at a fixed value.
///
/// The ten key columns are written **from the key** rather than typed twice, so a
/// case that moves an axis on the key moves the column the comparators read and
/// the two cannot drift apart in this fixture.
fn row_of(key: &ScopeKey) -> price::Model {
    price::Model {
        price_id: uuid::Uuid::from_u128(0x_11),
        tenant_id: uuid::Uuid::from_u128(0x_7e),
        plan_id: key.plan_id().get(),
        currency: key.currency().as_str().to_owned(),
        region: key.region().as_str().to_owned(),
        price_overlay: key.price_overlay().as_str().to_owned(),
        phase: key.phase().get(),
        price_eligibility: key.price_eligibility().as_str().to_owned(),
        charge_kind: key.charge_kind().as_str().to_owned(),
        cohort: key.cohort().to_string(),
        amount_minor: None,
        unit_rate_nano: None,
        model_kind: Some("graduated".to_owned()),
        tax_inclusive: false,
        tax_category_ref: None,
        resolved_tax_category: None,
        resolved_rounding_policy: None,
        billing_timing: None,
        billing_anchor_policy: None,
        anchor_day: None,
        proration_basis: None,
        credit_on_downgrade: None,
        quantity_source: None,
        manual_quantity: None,
        package_size: None,
        package_price_minor: None,
        meter: key.meter().map(|meter| meter.as_str().to_owned()),
        dimension_key: key.dimension_key().as_str().to_owned(),
        billing_granularity: Some("whole_unit".to_owned()),
        aggregation_function: None,
        aggregation_granularity: None,
        tier_aggregation_window: Some("calendar_month".to_owned()),
        tier_qualification_window: None,
        max_hold_granules: None,
        included_allowance: None,
        reserved_rate_nano: None,
        reservation_flavor: None,
        min_qty_purchase: None,
        min_qty_usage: None,
        min_qty_usage_fallback: None,
        discount_ref: None,
        rounding_policy_ref: Some("half_up".to_owned()),
        grandfather_until: None,
        supersedes_price_id: None,
        lifecycle_state: "published".to_owned(),
        created_by: ACTOR,
        created_at_utc: utc_ymd_hms(2099, 8, 5, 0, 0, 0),
        row_version: 0,
    }
}

/// One axis, the row that moves it, and whether [`market_columns`] is supposed to
/// see the move.
struct AxisCase {
    /// The axis's name in the design set's spelling, for the failure message.
    axis: &'static str,
    /// **Does the market comparator compare it?** Two of the ten are outside it by
    /// design, and naming which two here is what makes this file a statement about
    /// `market_columns`' contract rather than a demand that it compare everything.
    in_market: bool,
    /// The base row with that axis, and only that axis, moved.
    moved: price::Model,
}

/// One case per axis of the canonical scope key.
///
/// **The destructure is the gate.** `let ScopeKeyParts { … } = key.parts()` carries
/// no rest pattern, so an eleventh axis makes this pattern non-exhaustive and this
/// file stops compiling — in the same commit that adds the axis, and pointing at
/// the list below that has to grow with it. That is `approval_repo_tests`' own
/// pattern (a probe whose operand moves independently of the thing it checks)
/// applied to a comparator that has no other cover at all.
///
/// Each binding is then **read**, against the base row it was taken from, so a
/// field that someone adds to the pattern and forgets to give a case is a failing
/// assertion rather than an unused binding.
fn axis_cases() -> Vec<AxisCase> {
    let key = base_key();
    let base = row_of(&key);
    let moved = |mutate: fn(&mut price::Model)| {
        let mut row = base.clone();
        mutate(&mut row);
        row
    };

    let ScopeKeyParts {
        plan_id,
        currency,
        region,
        price_overlay,
        phase,
        price_eligibility,
        charge_kind,
        cohort,
        meter,
        dimension_key,
    } = key.parts();

    // Every binding read against the column it projects onto: the fixture claims
    // `row_of` writes the key's ten axes into the row's ten columns, and this is
    // where that claim is checked rather than assumed.
    assert_eq!(base.plan_id, plan_id.get());
    assert_eq!(base.currency, currency.as_str());
    assert_eq!(base.region, region.as_str());
    assert_eq!(base.price_overlay, price_overlay.as_str());
    assert_eq!(base.phase, phase.get());
    assert_eq!(base.price_eligibility, price_eligibility.as_str());
    assert_eq!(base.charge_kind, charge_kind.as_str());
    assert_eq!(base.cohort, cohort.to_string());
    assert_eq!(base.meter.as_deref(), meter.map(Meter::as_str));
    assert_eq!(base.dimension_key, dimension_key.as_str());

    vec![
        AxisCase {
            axis: "planId",
            in_market: true,
            moved: moved(|row| row.plan_id = OTHER_PLAN),
        },
        AxisCase {
            axis: "currency",
            in_market: true,
            moved: moved(|row| "USD".clone_into(&mut row.currency)),
        },
        AxisCase {
            axis: "region",
            in_market: true,
            moved: moved(|row| "us".clone_into(&mut row.region)),
        },
        AxisCase {
            axis: "priceOverlay",
            in_market: true,
            moved: moved(|row| "partner".clone_into(&mut row.price_overlay)),
        },
        AxisCase {
            axis: "phase",
            in_market: true,
            moved: moved(|row| row.phase = OTHER_PHASE),
        },
        // **The two `market_columns` deliberately does not compare.** A cutover's
        // copy moves exactly these on its way to a new generation, so seeing them
        // would make `refuse_ungenerational` refuse every legal copy.
        AxisCase {
            axis: "priceEligibility",
            in_market: false,
            moved: moved(|row| {
                PriceEligibility::AllSubscriptions
                    .as_str()
                    .clone_into(&mut row.price_eligibility);
            }),
        },
        AxisCase {
            axis: "chargeKind",
            in_market: true,
            moved: moved(|row| {
                ChargeKind::Recurring
                    .as_str()
                    .clone_into(&mut row.charge_kind);
            }),
        },
        AxisCase {
            axis: "cohort",
            in_market: false,
            moved: moved(|row| Cohort::None.to_string().clone_into(&mut row.cohort)),
        },
        AxisCase {
            axis: "meter",
            in_market: true,
            moved: moved(|row| row.meter = Some("api_bytes".to_owned())),
        },
        AxisCase {
            axis: "dimensionKey",
            in_market: true,
            moved: moved(|row| "region=us".clone_into(&mut row.dimension_key)),
        },
    ]
}

#[test]
fn the_canonical_comparator_sees_every_axis_of_the_key() {
    let base = row_of(&base_key());
    for case in axis_cases() {
        assert_ne!(
            scope_key_columns(&base),
            scope_key_columns(&case.moved),
            "`scope_key_columns` claims to be the whole canonical key and does not compare \
             {}; the last time it was short of an axis, `refuse_mispaired` read a successor on \
             a different meter of the same market as being on the predecessor's key",
            case.axis
        );
    }
}

#[test]
fn the_market_comparator_sees_every_axis_but_the_two_a_generation_moves() {
    let base = row_of(&base_key());
    for case in axis_cases() {
        let differs = market_columns(&base) != market_columns(&case.moved);
        assert_eq!(
            differs,
            case.in_market,
            "`market_columns` compares the key modulo `priceEligibility` and `cohort`, so it \
             must {} a move of {}. It is `refuse_ungenerational`'s operand — the row plane's \
             last guard before `publish_rows` — so an axis it cannot see admits a grandfathered \
             copy onto a key nobody composed, and an axis it should not see refuses every legal \
             cutover",
            if case.in_market { "see" } else { "ignore" },
            case.axis
        );
    }
}

#[test]
fn the_two_comparators_differ_by_exactly_the_generation_axes() {
    // The pair of the two cases above, stated as one claim so that a future
    // widening of `market_columns` to "modulo three axes" — the shape its own doc
    // makes invisible — has to move a number here as well as a tuple there.
    let excluded: Vec<&'static str> = axis_cases()
        .iter()
        .filter(|case| !case.in_market)
        .map(|case| case.axis)
        .collect();
    assert_eq!(
        excluded,
        vec!["priceEligibility", "cohort"],
        "`market_columns`' own doc says `all of them but priceEligibility and cohort`, and a \
         third exclusion is the shape a missing axis wears when it hides in this function"
    );
}

// ---------------------------------------------------------------------------
// The history walk's keyset predicate (review Z2-10)
// ---------------------------------------------------------------------------

/// [`super::after_history_position`] rendered on both engines.
///
/// `list_history_page` is the surface `api::rest::history` pages the tenant's
/// whole price history through, and this predicate is the whole of what makes
/// that walk total. Until it was lifted out it was reachable only by driving a
/// store, and its failure mode is the one a store-driven fixture is least likely
/// to build: **two rows sharing an authoring instant**. Every seeded fixture in
/// this gear writes rows at distinct instants unless it sets out not to, so a
/// degradation to a bare `>` on the instant loses rows and every existing case
/// stays green.
///
/// `audit_repo_tests::the_cursor_predicate_is_the_same_three_tier_shape_on_both_engines`
/// is the twin of this case one table over, and asserts the same two properties
/// for the same reasons.
#[test]
fn the_history_cursor_breaks_a_shared_instant_by_price_id_on_both_engines() {
    use sea_orm::sea_query::{PostgresQueryBuilder, Query, SqliteQueryBuilder};

    let mut select = Query::select();
    select
        .expr(sea_orm::sea_query::Expr::cust("1"))
        .cond_where(super::after_history_position(super::HistoryPosition {
            authored_at: from_unix(1_700_000_000, 0).expect("a valid instant"),
            price_id: uuid::Uuid::from_u128(0xb0b),
        }));
    let sqlite = select.to_string(SqliteQueryBuilder);
    let postgres = select.to_string(PostgresQueryBuilder);

    for (engine, sql) in [("sqlite", &sqlite), ("postgres", &postgres)] {
        assert!(
            sql.contains("\"created_at_utc\" >"),
            "{engine}: the instant must advance strictly, or a page re-serves its own last row: \
             {sql}"
        );
        assert!(
            sql.contains("\"created_at_utc\" =") && sql.contains("\"price_id\" >"),
            "{engine}: rows sharing the cursor's instant must be broken by price_id, or the walk \
             skips every one of them and the history has a hole no reader can see: {sql}"
        );
        assert!(
            !sql.contains("(\"created_at_utc\", "),
            "{engine}: a row-value comparison is engine-dependent, which is what writing the \
             OR-of-ANDs out avoids: {sql}"
        );
    }

    assert_eq!(
        sqlite, postgres,
        "one predicate, not two: a shape that differs between the engines makes the walk's \
         totality a deployment property"
    );
}

// ---------------------------------------------------------------------------
// The update guard's two sides (wave A F3, 2026-08-21)
// ---------------------------------------------------------------------------

/// The submitted line the caller hands [`check_update_keeps_the_line`],
/// taken from the key's own axes.
///
/// **Not typed out.** `update_draft` builds it with
/// `price_record::canonical_usage_line`, whose whole contract is that the pair it
/// renders is the pair the key's axes hold; reading it off `key` here is that
/// contract used as the fixture, so a case cannot be armed against a spelling the
/// key never has.
fn submitted_line(key: &ScopeKey) -> (Option<String>, String) {
    (
        key.meter().map(|meter| meter.as_str().to_owned()),
        key.dimension_key().as_str().to_owned(),
    )
}

/// **A stray space in the stored column is not a move of the line.**
///
/// `content_model` is the single normalizing render of the two columns, and it
/// normalizes the *submitted* side of this guard. The stored side stays raw, which
/// inverts the failure rather than closing it: a
/// row written before that rendering existed holds `"api_calls "` in the column
/// while its key axis holds `api_calls`, so a `PATCH` resubmitting the row's own
/// line verbatim was answered `USAGE_LINE_AXIS_MISMATCH` — naming a move nobody
/// made, and unrepairable, because the rule this function states is that an
/// update may not move the line.
///
/// A named column and a mutation that respells or moves it on a stored row.
///
/// A `type` rather than the tuple written twice: `clippy::type_complexity` is
/// denied workspace-wide, and the two arrays below are the same shape by
/// intent — one respelling, one moving — so they should read as one shape.
type StoredLineEdit = (&'static str, fn(&mut price::Model));

/// Both columns are exercised: `meter` is an `Option` and `dimension_key` is a
/// total `''`-defaulting column, and they are normalized by two different
/// primitives (`Meter::normalized`, `DimensionKey::new`), so a fix that reached
/// only one of them passes half of this.
#[test]
fn the_update_guard_reads_a_raw_stored_column_as_the_line_it_spells() {
    let key = base_key();
    let submitted = submitted_line(&key);

    let respelled: [StoredLineEdit; 2] = [
        ("meter", |row| row.meter = Some(" api_calls ".to_owned())),
        ("dimensionKey", |row| {
            " region=eu ".clone_into(&mut row.dimension_key);
        }),
    ];

    for (column, respell) in respelled {
        let mut row = row_of(&key);
        respell(&mut row);
        check_update_keeps_the_line(&row, &submitted).unwrap_or_else(|e| {
            panic!(
                "a row whose stored `{column}` differs from its key axis only by whitespace is \
                 on the line it is filed under, and a PATCH that resubmits that line must be \
                 admitted rather than frozen out of every later edit: {e}"
            )
        });
    }
}

/// The positive control the case above needs: **a real move is still refused.**
///
/// Normalizing the stored side is only safe if it cannot swallow a move of the
/// axes themselves, so each column is moved to a different value — not a
/// different spelling of the same one — and the refusal must be the guard's own.
#[test]
fn the_update_guard_still_refuses_a_line_that_actually_moves() {
    let key = base_key();
    let submitted = submitted_line(&key);

    let moved: [StoredLineEdit; 3] = [
        ("meter", |row| row.meter = Some("api_bytes".to_owned())),
        ("dimensionKey", |row| {
            "region=us".clone_into(&mut row.dimension_key);
        }),
        // The `None`/`Some` edge, which no trim can reach: an unmetered line is a
        // different line from a metered one, not a blanker spelling of it.
        ("meter absent", |row| row.meter = None),
    ];

    for (column, move_it) in moved {
        let mut row = row_of(&key);
        move_it(&mut row);
        let err = check_update_keeps_the_line(&row, &submitted).expect_err(
            "an update that moves the row's usage line must be refused: the pair are two axes \
             of the key the row is filed under",
        );
        assert!(
            matches!(err, RepoError::UsageLineDisagrees { .. }),
            "and by the reason the door renders as USAGE_LINE_AXIS_MISMATCH, not by some \
             neighbouring refusal ({column}): {err}"
        );
    }
}

// ---------------------------------------------------------------------------
// A corrupt axis names the row (wave B observation, 2026-08-21)
// ---------------------------------------------------------------------------

/// **An axis the loader cannot read names the row it could not read.**
///
/// `to_scope_key`'s refusals fold into `RepoError::CorruptRow`, which the door
/// renders as `DomainError::Internal` — so **one** unreadable row answers `500`
/// for the whole listing it appears in. Until the row id was in the message the
/// operator was told which column and not which row, which on a plan of two
/// hundred prices names no repair at all.
///
/// The class is not hypothetical. `pricing_price.region` and `.meter` carry no
/// `CHECK` on either engine, while `Region::new` and `Meter::new` refuse the
/// canonical key's segment separator — so the store admits a
/// spelling the loader refuses, and this gear's own earlier history could write
/// one. The chain cannot repair it either: the toolkit runner skips any migration
/// already in its ledger, so a normalization added in place to an applied
/// migration never runs on the databases that would need it.
///
/// `overlay_repo_tests::a_stored_currency_the_domain_cannot_read_is_a_corrupt_row`
/// is this case one door over, and states the same standard in the same words.
#[test]
fn an_unreadable_axis_names_the_row_it_could_not_read() {
    let key = base_key();

    let mut row = row_of(&key);
    "e|u".clone_into(&mut row.region);
    let err = to_scope_key(&row)
        .expect_err("a region carrying the canonical key's separator is not a readable axis");
    let RepoError::CorruptRow(detail) = &err else {
        panic!("an unreadable axis is a corrupt row, not some other failure: {err}");
    };
    assert!(
        detail.contains("pricing_price.region"),
        "the refusal must name the column: {detail}"
    );
    assert!(
        detail.contains(&row.price_id.to_string()),
        "and the row, because the caller is a listing and the answer is a 500 over all of it: \
         {detail}"
    );

    // The other constructor that refuses the separator, and the one the usage
    // line reads — it is attached in the `and_then` after the key is built, so a
    // wrapper that only covered the early returns would miss it.
    let mut row = row_of(&key);
    row.meter = Some("api|calls".to_owned());
    let err = to_scope_key(&row).expect_err("nor is such a meter");
    let RepoError::CorruptRow(detail) = &err else {
        panic!("an unreadable meter is a corrupt row too: {err}");
    };
    assert!(
        detail.contains(&row.price_id.to_string()),
        "the usage-line arm must name the row as well: {detail}"
    );

    // The control: an ordinary row still rehydrates, and onto its own axes. A
    // wrapper that refused everything would satisfy both halves above.
    let row = row_of(&key);
    let loaded = to_scope_key(&row).expect("the fixture's own row must read back");
    assert_eq!(loaded.region().as_str(), key.region().as_str());
    assert_eq!(
        loaded.meter().map(Meter::as_str),
        key.meter().map(Meter::as_str)
    );
    assert_eq!(
        loaded.dimension_key().as_str(),
        key.dimension_key().as_str()
    );
}
