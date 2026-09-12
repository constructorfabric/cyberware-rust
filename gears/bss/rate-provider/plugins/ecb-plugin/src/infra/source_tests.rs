//! ECB parser + mapping tests over the daily-XML fixture.

use bss_ledger_sdk::{CurrencyPair, RateProviderError};
use chrono::{Datelike, NaiveDate};
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time};

use super::{ecb_rates_to_provider_rates, parse_ecb_xml};

const FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/eurofxref-daily.xml");

/// The `provider_id` the mapper stamps onto every rate under test.
const PROVIDER: &str = "ecb";

fn midnight_utc(year: i32, month: u8, day: u8) -> OffsetDateTime {
    let month = Month::try_from(month).unwrap();
    let date = Date::from_calendar_date(year, month, day).unwrap();
    PrimitiveDateTime::new(date, Time::MIDNIGHT).assume_utc()
}

#[test]
fn parses_date_and_all_pairs() {
    let (date, raw) = parse_ecb_xml(FIXTURE).unwrap();
    assert_eq!((date.year(), date.month(), date.day()), (2026, 7, 21));
    assert_eq!(raw.len(), 3);
    assert!(raw.iter().any(|(c, r)| c == "USD" && r == "1.0856"));
}

/// chrono's `%Y-%m-%d` accepts a signed year past 9999 and `time` does not hold
/// it. On main the far-future `as_of` was refused by the publication-time guard;
/// an epoch fallback would pass that guard and store the feed permanently stale,
/// so the date is refused at the parse stage instead.
#[test]
fn a_publication_date_time_cannot_hold_is_refused_not_the_epoch() {
    let date = NaiveDate::from_ymd_opt(12345, 1, 1).unwrap();
    let raw = vec![("USD".to_owned(), "1.0856".to_owned())];
    let err = ecb_rates_to_provider_rates(date, &raw, &[], PROVIDER).unwrap_err();
    assert!(
        matches!(&err, RateProviderError::Internal(detail) if detail.contains("12345-01-01")),
        "{err:?}"
    );
}

#[test]
fn whole_table_when_no_pairs_requested() {
    let (date, raw) = parse_ecb_xml(FIXTURE).unwrap();
    let rates = ecb_rates_to_provider_rates(date, &raw, &[], PROVIDER).unwrap();
    assert_eq!(rates.len(), 3);
    let usd = rates.iter().find(|r| r.quote == "USD").unwrap();
    assert_eq!(usd.base, "EUR");
    assert_eq!(usd.rate_micro, 1_085_600);
    assert_eq!(usd.as_of, midnight_utc(2026, 7, 21));
}

#[test]
fn every_rate_is_stamped_with_the_serving_provider_id() {
    // Provenance is per-rate, not read back from a separate `provider_id()` call,
    // so the ledger records the true upstream on every stored row.
    let (date, raw) = parse_ecb_xml(FIXTURE).unwrap();
    let rates = ecb_rates_to_provider_rates(date, &raw, &[], "bank-x").unwrap();
    assert!(!rates.is_empty());
    assert!(rates.iter().all(|r| r.provider == "bank-x"));
}

#[test]
fn requested_pair_filters_and_omits_unavailable() {
    let (date, raw) = parse_ecb_xml(FIXTURE).unwrap();
    let want = vec![
        CurrencyPair {
            base: "EUR".to_owned(),
            quote: "USD".to_owned(),
        },
        CurrencyPair {
            base: "EUR".to_owned(),
            quote: "CHF".to_owned(),
        },
    ];
    let rates = ecb_rates_to_provider_rates(date, &raw, &want, PROVIDER).unwrap();
    assert_eq!(rates.len(), 1);
    assert_eq!(rates[0].quote, "USD");
}

#[test]
fn non_xml_input_is_internal_error() {
    assert!(matches!(
        parse_ecb_xml(b"not xml"),
        Err(RateProviderError::Internal(_))
    ));
}

#[test]
fn well_formed_xml_without_cube_entries_is_internal_error() {
    // Structurally valid XML carrying neither a publication date nor rate entries.
    assert!(matches!(
        parse_ecb_xml(b"<a></a>"),
        Err(RateProviderError::Internal(_))
    ));
}

#[test]
fn case_insensitive_pair_request_still_matches() {
    // ECB's own codes are uppercase; a caller that didn't normalize casing (e.g.
    // "usd") must still match the published pair.
    let (date, raw) = parse_ecb_xml(FIXTURE).unwrap();
    let want = vec![CurrencyPair {
        base: "eur".to_owned(),
        quote: "usd".to_owned(),
    }];
    let rates = ecb_rates_to_provider_rates(date, &raw, &want, PROVIDER).unwrap();
    assert_eq!(rates.len(), 1);
    assert_eq!(rates[0].quote, "USD");
}

#[test]
fn reversed_pair_is_omitted_not_inverted() {
    // ECB only publishes EUR->X. The inverse leg (X->EUR, which a
    // non-EUR-functional tenant's ledger needs) must never be synthesized here —
    // that inversion is the ledger's triangulation job. With no other requested
    // pair left, the pair-unavailable error lets the composite try a source that
    // may publish the leg directly.
    let (date, raw) = parse_ecb_xml(FIXTURE).unwrap();
    let want = vec![CurrencyPair {
        base: "USD".to_owned(),
        quote: "EUR".to_owned(),
    }];
    let err = ecb_rates_to_provider_rates(date, &raw, &want, PROVIDER).unwrap_err();
    assert!(
        matches!(&err, RateProviderError::PairUnavailable { base, quote } if base == "USD" && quote == "EUR"),
        "expected PairUnavailable naming the requested leg, got {err:?}"
    );
}

#[test]
fn refetching_the_identical_document_is_deterministic() {
    // A non-publication day (weekend/holiday) re-serves the same document
    // unchanged; the adapter must never fabricate a fresher `as_of`.
    let (date_a, raw_a) = parse_ecb_xml(FIXTURE).unwrap();
    let (date_b, raw_b) = parse_ecb_xml(FIXTURE).unwrap();
    let rates_a = ecb_rates_to_provider_rates(date_a, &raw_a, &[], PROVIDER).unwrap();
    let rates_b = ecb_rates_to_provider_rates(date_b, &raw_b, &[], PROVIDER).unwrap();
    assert_eq!(rates_a, rates_b);
}

#[test]
fn duplicate_currency_entry_keeps_first_and_does_not_error() {
    const DUPLICATE_CURRENCY_XML: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<gesmes:Envelope xmlns:gesmes="http://www.gesmes.org/xml/2002-08-01" xmlns="http://www.ecb.int/vocabulary/2002-08-01/eurofxref">
  <Cube>
    <Cube time="2026-07-21">
      <Cube currency="USD" rate="1.0856"/>
      <Cube currency="USD" rate="1.2000"/>
    </Cube>
  </Cube>
</gesmes:Envelope>
"#;
    let (_date, raw) = parse_ecb_xml(DUPLICATE_CURRENCY_XML).unwrap();
    let usd_entries: Vec<_> = raw.iter().filter(|(c, _)| c == "USD").collect();
    assert_eq!(
        usd_entries.len(),
        1,
        "duplicate currency entry must be deduped, not doubled"
    );
    assert_eq!(usd_entries[0].1, "1.0856", "the first occurrence must win");
}

#[test]
fn a_second_distinct_date_fails_the_whole_document() {
    // The daily feed is one day's rates, so two dates means this is some other
    // file — most likely the 90-day history feed behind a mistyped `base_url`.
    // Picking by document order would quietly serve a subset of the wrong file.
    const MULTI_DATE_XML: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<gesmes:Envelope xmlns:gesmes="http://www.gesmes.org/xml/2002-08-01" xmlns="http://www.ecb.int/vocabulary/2002-08-01/eurofxref">
  <Cube>
    <Cube time="2026-07-21">
      <Cube currency="USD" rate="1.0856"/>
    </Cube>
    <Cube time="2026-07-20">
      <Cube currency="JPY" rate="160.85"/>
    </Cube>
  </Cube>
</gesmes:Envelope>
"#;
    let err = parse_ecb_xml(MULTI_DATE_XML).unwrap_err();
    assert!(
        matches!(err, RateProviderError::Internal(ref m)
            if m.contains("2026-07-21") && m.contains("2026-07-20")),
        "the rejection must name both dates so the operator can see which files collided: {err:?}"
    );
}

#[test]
fn a_repeated_identical_date_is_not_a_conflict() {
    // The same date stated twice is redundant, not contradictory, and every row
    // still belongs to it — failing would reject a valid, if odd, feed.
    const REPEATED_DATE_XML: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<gesmes:Envelope xmlns:gesmes="http://www.gesmes.org/xml/2002-08-01" xmlns="http://www.ecb.int/vocabulary/2002-08-01/eurofxref">
  <Cube>
    <Cube time="2026-07-21">
      <Cube currency="USD" rate="1.0856"/>
    </Cube>
    <Cube time="2026-07-21">
      <Cube currency="JPY" rate="160.85"/>
    </Cube>
  </Cube>
</gesmes:Envelope>
"#;
    let (date, raw) = parse_ecb_xml(REPEATED_DATE_XML).unwrap();
    assert_eq!((date.year(), date.month(), date.day()), (2026, 7, 21));
    assert_eq!(raw.len(), 2, "both blocks' rows belong to the same date");
}

#[test]
fn a_self_closing_dated_cube_still_establishes_the_publication_date() {
    // A `<Cube time="…"/>` that encloses nothing is a different XML event than an
    // opening tag, so the date has to be read on both. Otherwise the document's
    // only date could be silently skipped and the parse would fail as "missing
    // publication date" against a feed that plainly states one.
    const SELF_CLOSING_DATE_XML: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<gesmes:Envelope xmlns:gesmes="http://www.gesmes.org/xml/2002-08-01" xmlns="http://www.ecb.int/vocabulary/2002-08-01/eurofxref">
  <Cube>
    <Cube time="2026-07-21"/>
    <Cube time="2026-07-21">
      <Cube currency="USD" rate="1.0856"/>
    </Cube>
  </Cube>
</gesmes:Envelope>
"#;
    let (date, raw) = parse_ecb_xml(SELF_CLOSING_DATE_XML).unwrap();
    assert_eq!(date, NaiveDate::from_ymd_opt(2026, 7, 21).unwrap());
    // The empty block carries no rows of its own; the real block's row survives.
    assert_eq!(raw.len(), 1);
    assert_eq!(raw[0].0, "USD");
}

#[test]
fn an_unrecognised_cube_attribute_is_ignored() {
    // ECB has added attributes to this feed before. An unknown one must be passed
    // over rather than derailing the attribute scan, so a feed extension does not
    // turn into a parse failure that blocks every FX post.
    const EXTRA_ATTRIBUTE_XML: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<gesmes:Envelope xmlns:gesmes="http://www.gesmes.org/xml/2002-08-01" xmlns="http://www.ecb.int/vocabulary/2002-08-01/eurofxref">
  <Cube>
    <Cube time="2026-07-21" source="ecb">
      <Cube currency="USD" rate="1.0856" revision="2"/>
    </Cube>
  </Cube>
</gesmes:Envelope>
"#;
    let (date, raw) = parse_ecb_xml(EXTRA_ATTRIBUTE_XML).unwrap();
    assert_eq!(date, NaiveDate::from_ymd_opt(2026, 7, 21).unwrap());
    assert_eq!(raw.len(), 1);
    assert_eq!(raw[0], ("USD".to_owned(), "1.0856".to_owned()));
}

#[test]
fn non_iso4217_shaped_currency_is_dropped_not_stored() {
    // The feed is a system boundary: a compromised/misconfigured upstream must
    // not get an arbitrary string into a tenant's FX store as a currency code.
    const INJECTED_XML: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<gesmes:Envelope xmlns:gesmes="http://www.gesmes.org/xml/2002-08-01" xmlns="http://www.ecb.int/vocabulary/2002-08-01/eurofxref">
  <Cube>
    <Cube time="2026-07-21">
      <Cube currency="USD" rate="1.0856"/>
      <Cube currency="NOT-A-CURRENCY" rate="1.0"/>
      <Cube currency="U$D" rate="1.0"/>
      <Cube currency="" rate="1.0"/>
    </Cube>
  </Cube>
</gesmes:Envelope>
"#;
    let (_date, raw) = parse_ecb_xml(INJECTED_XML).unwrap();
    assert_eq!(raw.len(), 1, "only the well-formed code survives");
    assert_eq!(raw[0].0, "USD");
}

#[test]
fn lowercase_feed_currency_is_normalized_to_uppercase() {
    // Keeps the stored code canonical so the ledger never holds both "usd" and
    // "USD" rows for one currency.
    const LOWERCASE_XML: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<gesmes:Envelope xmlns:gesmes="http://www.gesmes.org/xml/2002-08-01" xmlns="http://www.ecb.int/vocabulary/2002-08-01/eurofxref">
  <Cube>
    <Cube time="2026-07-21">
      <Cube currency="usd" rate="1.0856"/>
    </Cube>
  </Cube>
</gesmes:Envelope>
"#;
    let (_date, raw) = parse_ecb_xml(LOWERCASE_XML).unwrap();
    assert_eq!(raw.len(), 1);
    assert_eq!(raw[0].0, "USD");
}

#[test]
fn all_currencies_malformed_yields_no_rate_entries_error() {
    // Dropping every entry must surface as an error, never a silent empty `Ok`.
    const ALL_BAD_XML: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<gesmes:Envelope xmlns:gesmes="http://www.gesmes.org/xml/2002-08-01" xmlns="http://www.ecb.int/vocabulary/2002-08-01/eurofxref">
  <Cube>
    <Cube time="2026-07-21">
      <Cube currency="BAD-CODE" rate="1.0"/>
    </Cube>
  </Cube>
</gesmes:Envelope>
"#;
    assert!(matches!(
        parse_ecb_xml(ALL_BAD_XML),
        Err(RateProviderError::Internal(_))
    ));
}

#[test]
fn one_unconvertible_rate_does_not_cost_the_whole_document() {
    // The ledger's RateSyncJob turns ANY configured-provider failure into
    // FX_SNAPSHOT_MISSING and refreshes nothing, so failing the batch over a
    // single bad value would stall the FX fan-out for every other currency.
    let raw = vec![
        ("USD".to_owned(), "1.0856".to_owned()),
        ("JPY".to_owned(), "not-a-number".to_owned()),
        ("GBP".to_owned(), "0.84".to_owned()),
    ];
    let date = NaiveDate::from_ymd_opt(2026, 7, 21).unwrap();
    let rates = ecb_rates_to_provider_rates(date, &raw, &[], PROVIDER).unwrap();
    assert_eq!(rates.len(), 2, "the two good entries must still be served");
    assert!(rates.iter().all(|r| r.quote != "JPY"));
}

#[test]
fn a_non_positive_rate_is_skipped_like_any_other_unconvertible_value() {
    // Zero/negative rates are rejected by the converter (there are none in this
    // domain); that rejection must be per-entry, not fatal for the batch.
    let raw = vec![
        ("USD".to_owned(), "1.0856".to_owned()),
        ("JPY".to_owned(), "0".to_owned()),
        ("CHF".to_owned(), "-1.5".to_owned()),
    ];
    let date = NaiveDate::from_ymd_opt(2026, 7, 21).unwrap();
    let rates = ecb_rates_to_provider_rates(date, &raw, &[], PROVIDER).unwrap();
    assert_eq!(rates.len(), 1);
    assert_eq!(rates[0].quote, "USD");
}

#[test]
fn all_rates_unconvertible_is_internal_error_not_an_empty_ok() {
    // An empty table passed off as success would read as a served tick and
    // suppress the composite's fallback.
    let raw = vec![
        ("USD".to_owned(), "nope".to_owned()),
        ("JPY".to_owned(), String::new()),
    ];
    let date = NaiveDate::from_ymd_opt(2026, 7, 21).unwrap();
    assert!(matches!(
        ecb_rates_to_provider_rates(date, &raw, &[], PROVIDER),
        Err(RateProviderError::Internal(_))
    ));
}

#[test]
fn pair_filter_excluding_everything_is_pair_unavailable() {
    // Nothing was *attempted*, so this is "the feed does not publish what you
    // asked for", not a fault. It stays an `Err` so the composite advances to a
    // source that might publish the pair.
    let raw = vec![("USD".to_owned(), "1.0856".to_owned())];
    let date = NaiveDate::from_ymd_opt(2026, 7, 21).unwrap();
    let want = vec![CurrencyPair {
        base: "EUR".to_owned(),
        quote: "CHF".to_owned(),
    }];
    let err = ecb_rates_to_provider_rates(date, &raw, &want, PROVIDER).unwrap_err();
    assert!(
        matches!(&err, RateProviderError::PairUnavailable { base, quote } if base == "EUR" && quote == "CHF"),
        "expected PairUnavailable naming the requested pair, got {err:?}"
    );
}

#[test]
fn no_requested_pairs_and_an_empty_feed_stays_ok() {
    // The whole-table call the ledger actually makes: with `pairs` empty there is
    // no pair to report as unavailable, so an empty feed is a served document
    // with nothing in it, not an error.
    let date = NaiveDate::from_ymd_opt(2026, 7, 21).unwrap();
    let rates = ecb_rates_to_provider_rates(date, &[], &[], PROVIDER).unwrap();
    assert!(rates.is_empty());
}

#[test]
fn cube_with_only_one_of_currency_or_rate_is_ignored() {
    // A truncated/malformed feed row: unusable, and must not be recorded.
    const HALF_PAIR_XML: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<gesmes:Envelope xmlns:gesmes="http://www.gesmes.org/xml/2002-08-01" xmlns="http://www.ecb.int/vocabulary/2002-08-01/eurofxref">
  <Cube>
    <Cube time="2026-07-21">
      <Cube currency="USD" rate="1.0856"/>
      <Cube currency="JPY"/>
      <Cube rate="160.85"/>
    </Cube>
  </Cube>
</gesmes:Envelope>
"#;
    let (_date, raw) = parse_ecb_xml(HALF_PAIR_XML).unwrap();
    assert_eq!(raw.len(), 1, "only the complete entry survives");
    assert_eq!(raw[0].0, "USD");
}

#[test]
fn currency_row_after_the_date_block_closes_is_omitted() {
    // GBP is a SIBLING of the date Cube, not nested inside it, so it was never
    // published under 2026-07-21. Comparing dates alone cannot catch this, because
    // `current_date` outlives the element it came from.
    const SIBLING_AFTER_BLOCK_XML: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<gesmes:Envelope xmlns:gesmes="http://www.gesmes.org/xml/2002-08-01" xmlns="http://www.ecb.int/vocabulary/2002-08-01/eurofxref">
  <Cube>
    <Cube time="2026-07-21">
      <Cube currency="USD" rate="1.0856"/>
    </Cube>
    <Cube currency="GBP" rate="0.8600"/>
  </Cube>
</gesmes:Envelope>
"#;
    let (date, raw) = parse_ecb_xml(SIBLING_AFTER_BLOCK_XML).unwrap();
    assert_eq!(date, NaiveDate::from_ymd_opt(2026, 7, 21).unwrap());
    assert_eq!(raw.len(), 1, "the out-of-block row must not be recorded");
    assert_eq!(raw[0].0, "USD");
}

#[test]
fn rows_under_an_undated_inner_block_are_omitted() {
    // GBP sits inside a Cube whose own `time` is unparseable, which is itself
    // still inside the dated block, so the depth check alone would keep it and
    // stamp 2026-07-21 onto a rate whose real publication date is unknown. Only
    // the "does this row's block carry the document's date?" check catches it.
    const UNDATED_INNER_XML: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<gesmes:Envelope xmlns:gesmes="http://www.gesmes.org/xml/2002-08-01" xmlns="http://www.ecb.int/vocabulary/2002-08-01/eurofxref">
  <Cube>
    <Cube time="2026-07-21">
      <Cube currency="USD" rate="1.0856"/>
      <Cube time="21-07-2026">
        <Cube currency="GBP" rate="0.8600"/>
      </Cube>
    </Cube>
  </Cube>
</gesmes:Envelope>
"#;
    let (date, raw) = parse_ecb_xml(UNDATED_INNER_XML).unwrap();
    assert_eq!(date, NaiveDate::from_ymd_opt(2026, 7, 21).unwrap());
    assert_eq!(raw.len(), 1, "only the dated block's own rows survive");
    assert_eq!(raw[0].0, "USD");
}

#[test]
fn unparseable_publication_date_is_reported_as_missing_not_silently_ignored() {
    // The only date in the feed is malformed, so parsing fails; the raw value is
    // logged (see `update_publication_date`).
    const BAD_DATE_XML: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<gesmes:Envelope xmlns:gesmes="http://www.gesmes.org/xml/2002-08-01" xmlns="http://www.ecb.int/vocabulary/2002-08-01/eurofxref">
  <Cube>
    <Cube time="21-07-2026">
      <Cube currency="USD" rate="1.0856"/>
    </Cube>
  </Cube>
</gesmes:Envelope>
"#;
    assert!(matches!(
        parse_ecb_xml(BAD_DATE_XML),
        Err(RateProviderError::Internal(_))
    ));
}
