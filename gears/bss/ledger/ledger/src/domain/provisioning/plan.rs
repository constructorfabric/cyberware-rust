//! Pure provisioning-plan logic: validate the fiscal-calendar fields and
//! derive the initial `period_id`. Free functions only (no `#[domain_model]`),
//! no infrastructure imports (DE0301).

use bss_ledger_sdk::{FiscalCalendarSpec, Granularity};

use crate::domain::error::DomainError;
use time::OffsetDateTime;

/// Validate a fiscal-calendar spec before any DB work: reject an empty
/// timezone, a fiscal-year start month outside `1..=12`, and (defensively)
/// any granularity other than `Month` (MVP supports monthly only).
///
/// # Errors
/// [`DomainError::InvalidRequest`] on an empty timezone, an out-of-range
/// `fy_start_month`, or an unsupported granularity.
pub fn validate_calendar(spec: &FiscalCalendarSpec) -> Result<(), DomainError> {
    if spec.timezone.is_empty() {
        return Err(DomainError::InvalidRequest(
            "fiscal calendar timezone must not be empty".to_owned(),
        ));
    }
    if !(1..=12).contains(&spec.fy_start_month) {
        return Err(DomainError::InvalidRequest(format!(
            "fiscal calendar fy_start_month must be 1..=12 (got {})",
            spec.fy_start_month
        )));
    }
    if spec.granularity != Granularity::Month {
        return Err(DomainError::InvalidRequest(
            "fiscal calendar granularity must be MONTH (MVP)".to_owned(),
        ));
    }
    // S5-F3 functional currency (optional — `None` = single-currency tenant). When
    // present it must be a plausible ISO-4217-ish code (non-empty, ASCII, ≤ 10),
    // the same envelope the FX-rate ingest + unallocated read use.
    if let Some(fc) = &spec.functional_currency
        && (fc.is_empty() || fc.len() > 10 || !fc.is_ascii())
    {
        return Err(DomainError::InvalidRequest(format!(
            "fiscal calendar functional_currency must be a non-empty ASCII code of at most \
             10 chars (got {fc:?})"
        )));
    }
    Ok(())
}

/// Derive the initial `period_id` from a UTC instant as `"%Y%m"` (decision 4 —
/// UTC-derived; tz-precise month boundaries are refined by the P6 automation).
#[must_use]
pub fn initial_period_id(now: OffsetDateTime) -> String {
    crate::domain::period::period_id_for(now)
}

#[cfg(test)]
mod tests {
    use crate::domain::instant::utc_ymd_hms;

    use super::*;

    fn spec(timezone: &str, granularity: Granularity, fy_start_month: u8) -> FiscalCalendarSpec {
        FiscalCalendarSpec {
            timezone: timezone.to_owned(),
            granularity,
            fy_start_month,
            functional_currency: None,
        }
    }

    fn spec_fc(functional_currency: Option<&str>) -> FiscalCalendarSpec {
        FiscalCalendarSpec {
            timezone: "UTC".to_owned(),
            granularity: Granularity::Month,
            fy_start_month: 1,
            functional_currency: functional_currency.map(ToOwned::to_owned),
        }
    }

    #[test]
    fn validate_calendar_accepts_valid_spec() {
        assert!(validate_calendar(&spec("UTC", Granularity::Month, 1)).is_ok());
        assert!(validate_calendar(&spec("Europe/Madrid", Granularity::Month, 12)).is_ok());
    }

    #[test]
    fn validate_calendar_rejects_empty_timezone() {
        assert!(matches!(
            validate_calendar(&spec("", Granularity::Month, 1)),
            Err(DomainError::InvalidRequest(_))
        ));
    }

    #[test]
    fn validate_calendar_rejects_out_of_range_fy_start_month() {
        assert!(matches!(
            validate_calendar(&spec("UTC", Granularity::Month, 0)),
            Err(DomainError::InvalidRequest(_))
        ));
        assert!(matches!(
            validate_calendar(&spec("UTC", Granularity::Month, 13)),
            Err(DomainError::InvalidRequest(_))
        ));
    }

    #[test]
    fn validate_calendar_accepts_functional_currency_or_none() {
        assert!(validate_calendar(&spec_fc(Some("USD"))).is_ok());
        assert!(
            validate_calendar(&spec_fc(None)).is_ok(),
            "a single-currency tenant (no functional currency) is valid"
        );
    }

    #[test]
    fn validate_calendar_rejects_malformed_functional_currency() {
        assert!(matches!(
            validate_calendar(&spec_fc(Some(""))),
            Err(DomainError::InvalidRequest(_))
        ));
        assert!(matches!(
            validate_calendar(&spec_fc(Some("THIS_IS_WAY_TOO_LONG"))),
            Err(DomainError::InvalidRequest(_))
        ));
    }

    #[test]
    fn initial_period_id_formats_year_month() {
        let now = utc_ymd_hms(2026, 6, 19, 10, 30, 0);
        assert_eq!(initial_period_id(now), "202606");
    }
}
