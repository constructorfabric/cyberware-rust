//! The shape rules §6 states, each refused on its own.
//!
//! Every case here is [`parse_entries`], which is the only place the five rules
//! that belong to the surface are decided. The two that belong to the version —
//! empty and duplicate — are `ThresholdVersion::new`'s and are asserted in
//! `domain::materiality_tests`, because they are refused inside the proposal's
//! transaction where the version number they would have consumed is still
//! unminted.

use super::{MAX_PERCENT_BP, ThresholdEntryView, parse_entries};
use crate::domain::error::DomainError;
use crate::domain::materiality::ThresholdBasis;
use time::OffsetDateTime;

/// One authored entry, with both bases spelled explicitly so a case that means to
/// set neither has to say so.
fn entry(
    currency: &str,
    absolute_minor: Option<i64>,
    percent_bp: Option<u32>,
) -> ThresholdEntryView {
    ThresholdEntryView {
        currency: currency.to_owned(),
        absolute_minor,
        percent_bp,
    }
}

/// The refusal's detail, or a panic naming what came back instead.
fn refusal(entries: &[ThresholdEntryView]) -> String {
    match parse_entries(entries) {
        Err(DomainError::ThresholdInvalid(detail)) => detail,
        other => panic!("expected THRESHOLD_INVALID, got {other:?}"),
    }
}

#[test]
fn a_well_formed_pair_of_entries_parses_and_normalizes_its_codes() {
    let parsed = parse_entries(&[entry("usd", Some(500), None), entry("EUR", None, Some(250))])
        .expect("both entries are well formed");
    assert_eq!(parsed.len(), 2);
    // Sorted by currency and uppercased: `usd` normalizes to `USD`, which sorts
    // after `EUR`. Both matter to the pin — the digest is taken over this exact
    // rendering — so a lowercase code that stayed lowercase would hash differently
    // from the same policy typed in capitals.
    assert_eq!(parsed[0].currency.as_str(), "EUR");
    assert_eq!(parsed[1].currency.as_str(), "USD");
    assert_eq!(parsed[0].basis, ThresholdBasis::Percent { bp: 250 });
    assert_eq!(parsed[1].basis, ThresholdBasis::Absolute { minor: 500 });
}

#[test]
fn the_entries_are_sorted_by_currency_whatever_order_they_were_typed_in() {
    // The property the sort exists for, stated as an equality between two orders
    // of one policy rather than as a claim about one of them. Without it the same
    // policy proposed twice pins two digests, and a reviewer who re-read the
    // proposal after a re-submit would be told the content had moved.
    let typed_one_way = parse_entries(&[
        entry("USD", Some(1), None),
        entry("EUR", Some(2), None),
        entry("GBP", Some(3), None),
    ])
    .expect("well formed");
    let typed_another = parse_entries(&[
        entry("GBP", Some(3), None),
        entry("USD", Some(1), None),
        entry("EUR", Some(2), None),
    ])
    .expect("well formed");
    assert_eq!(typed_one_way, typed_another);
}

#[test]
fn a_currency_that_is_not_iso_4217_alpha_3_is_threshold_invalid() {
    // And **not** `CURRENCY_INVALID`, which is what `CurrencyCode::new` answers.
    // The re-code is the assertion: a caller of this route has no price row, so a
    // `currency` precondition violation would name a field they cannot find. The
    // offending code is still in the detail, or the remedy is unreachable.
    let detail = refusal(&[entry("EURO", Some(1), None)]);
    assert!(
        detail.contains("EURO"),
        "the detail names the code: {detail}"
    );
    assert!(
        detail.contains("ISO 4217"),
        "and says what shape it wanted: {detail}"
    );
}

#[test]
fn every_refusal_names_its_field_the_way_the_wire_spells_it() {
    // `toolkit_macros::api_dto` emits `#[serde(rename_all = "snake_case")]`
    // unconditionally, so `ThresholdEntryView`'s wire members are `absolute_minor`
    // and `percent_bp`. Four refusals here named them in camelCase until
    // 2026-08-17, which pointed a caller at a field their own request does not
    // carry — the remedy unreachable in exactly the way the ISO 4217 case above
    // says it must not be.
    //
    // Asserted as a negative *and* a positive, because either alone is satisfiable
    // by a defect: a refusal that named nothing would pass the negative, and one
    // that named both spellings would pass the positive.
    for detail in [
        refusal(&[entry("EUR", None, None)]),
        refusal(&[entry("EUR", Some(1), Some(1))]),
        refusal(&[entry("EUR", Some(-1), None)]),
        refusal(&[entry("EUR", None, Some(0))]),
    ] {
        assert!(
            !detail.contains("absoluteMinor") && !detail.contains("percentBp"),
            "no refusal names a field the wire does not carry: {detail}"
        );
        assert!(
            detail.contains("absolute_minor") || detail.contains("percent_bp"),
            "and each names the one it is about: {detail}"
        );
    }
}

#[test]
fn an_entry_setting_neither_basis_is_threshold_invalid() {
    // §6's `{absolute_minor | percent}` is a choice, and "neither" is the arm that
    // switches the fail-safe off rather than the one that looks empty: the entry
    // still makes the currency *have* an entry, so `inst-mat-percurrency` stops
    // treating it as unconfigured while nothing thresholds it.
    let detail = refusal(&[entry("EUR", None, None)]);
    assert!(detail.contains("neither"), "got: {detail}");
}

#[test]
fn an_entry_setting_both_bases_is_threshold_invalid() {
    let detail = refusal(&[entry("EUR", Some(1), Some(1))]);
    assert!(detail.contains("both"), "got: {detail}");
}

#[test]
fn a_negative_absolute_threshold_is_threshold_invalid() {
    // §6's `absolute_minor >= 0`. A negative threshold is below every change there
    // is, which is the two-person rule switched off by arithmetic.
    let detail = refusal(&[entry("EUR", Some(-1), None)]);
    assert!(detail.contains("-1"), "got: {detail}");
}

#[test]
fn a_zero_absolute_threshold_is_accepted_and_that_is_deliberate() {
    // The boundary on the safe side, and it is a real configuration rather than an
    // oversight: zero means every change in that currency is material, which is a
    // tenant asking for more review and not less. `>= 0` is §6's own operator.
    let parsed = parse_entries(&[entry("EUR", Some(0), None)]).expect("zero is a threshold");
    assert_eq!(parsed[0].basis, ThresholdBasis::Absolute { minor: 0 });
}

#[test]
fn a_zero_percent_threshold_is_threshold_invalid() {
    // §6's `percent > 0`, verbatim, and the asymmetry with the case above is the
    // design set's rather than this surface's: zero **percent** would auto-publish
    // every change that moved by nothing at all, where zero absolute makes
    // everything material. The two zeroes sit on opposite sides of the fail-safe.
    let detail = refusal(&[entry("EUR", None, Some(0))]);
    assert!(detail.contains("1..="), "got: {detail}");
}

#[test]
fn a_percent_threshold_above_one_hundred_percent_is_threshold_invalid() {
    // The ceiling this surface decides (§6 names none), refused one basis point
    // above it so the boundary is asserted rather than a round number far outside.
    let detail = refusal(&[entry("EUR", None, Some(MAX_PERCENT_BP + 1))]);
    assert!(detail.contains("10001"), "got: {detail}");
    let parsed =
        parse_entries(&[entry("EUR", None, Some(MAX_PERCENT_BP))]).expect("100% is the ceiling");
    assert_eq!(
        parsed[0].basis,
        ThresholdBasis::Percent { bp: MAX_PERCENT_BP }
    );
}

#[test]
fn an_empty_entry_list_parses_here_and_is_refused_one_layer_in() {
    // Stated as a test rather than left to be inferred, because "the surface
    // accepts it" reads like a hole. It is not: the empty-set rule belongs to
    // `ThresholdVersion::new`, which runs inside the proposal's transaction — so
    // an empty proposal is refused `THRESHOLD_INVALID` there, *before* a version
    // number is minted and before a row is written. Refusing it here as well would
    // be the rule with two owners.
    assert_eq!(parse_entries(&[]).expect("parses"), Vec::new());

    // The second clause, which the name promised and the body did not carry: the
    // refusal one layer in. Asserted here as well as in `domain::materiality_tests`
    // because it is the half that makes the acceptance above safe rather than a
    // hole, and a reader of this file has no way to reach the other suite.
    let refusal =
        crate::domain::materiality::ThresholdVersion::new(0, OffsetDateTime::now_utc(), vec![])
            .expect_err("an empty set is refused where the rule lives");
    assert!(
        matches!(
            refusal,
            crate::domain::materiality::ThresholdRefusal::NoEntries
        ),
        "{refusal:?}"
    );
}
