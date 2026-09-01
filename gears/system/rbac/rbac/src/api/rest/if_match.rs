//! `If-Match` header parsing for the optimistic-concurrency endpoints
//! (subset of RFC 7232 §2.3 / §3.1):
//!
//! * Strong validators required — `W/` weak validators rejected with 400.
//! * Validators MAY be wrapped in double quotes, which are stripped.
//! * Surrounding ASCII whitespace is trimmed.
//! * `If-Match: *` is NOT supported — endpoints require a concrete validator.

use axum::http::{HeaderMap, header::IF_MATCH};
use toolkit::api::canonical_prelude::CanonicalError;

use crate::api::rest::error::rbac_service_error_to_canonical;
use crate::domain::etag::Etag;

/// Parse an optional `If-Match` header into an [`Etag`]. Returns
/// `Ok(None)` when absent, `Ok(Some(etag))` for a strong validator, or
/// `Err` (400 `InvalidArgument`) when malformed.
pub fn parse_if_match(headers: &HeaderMap) -> Result<Option<Etag>, CanonicalError> {
    let Some(value) = headers.get(IF_MATCH) else {
        return Ok(None);
    };
    let raw = value.to_str().map_err(|_| {
        rbac_service_error_to_canonical(rbac_sdk::error::RbacServiceError::validation(
            "If-Match header is not valid UTF-8",
        ))
    })?;
    let candidate = raw.trim();
    if candidate.is_empty() {
        return Err(rbac_service_error_to_canonical(
            rbac_sdk::error::RbacServiceError::validation("If-Match header is empty"),
        ));
    }
    if candidate == "*" {
        return Err(rbac_service_error_to_canonical(
            rbac_sdk::error::RbacServiceError::validation(
                "If-Match: * is not supported on this endpoint",
            ),
        ));
    }
    if let Some(rest) = candidate.strip_prefix("W/") {
        let _ = rest; // intentionally unused — weak validators are rejected
        return Err(rbac_service_error_to_canonical(
            rbac_sdk::error::RbacServiceError::validation(
                "If-Match: weak validators (W/\"\u{2026}\") are not supported",
            ),
        ));
    }
    // RFC 7232 §2.3: strong validators MAY appear quoted. Strip a single
    // pair of surrounding double quotes, if present.
    let unquoted = candidate
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(candidate);
    let etag: Etag = unquoted
        .parse()
        .map_err(|e: crate::domain::etag::EtagParseError| {
            // Preserve the typed parse error in logs — only the
            // formatted message reaches the wire (RFC 9457 detail), and
            // the typed variant carries which sub-rule failed.
            tracing::warn!(
                error = %e,
                error.kind = ?e,
                "If-Match header failed to parse as a strong ETag",
            );
            rbac_service_error_to_canonical(rbac_sdk::error::RbacServiceError::validation(format!(
                "If-Match: {e}"
            )))
        })?;
    Ok(Some(etag))
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue, header::IF_MATCH};

    use super::parse_if_match;
    use crate::domain::etag::etag_for;

    fn fixture_etag() -> String {
        let id = uuid::uuid!("11111111-2222-3333-4444-555555555555");
        let ts = chrono::DateTime::parse_from_rfc3339("2025-01-02T03:04:05.123456Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        etag_for(ts, id).as_str().to_owned()
    }

    fn headers_with(raw: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(IF_MATCH, HeaderValue::from_str(raw).unwrap());
        h
    }

    #[test]
    fn missing_header_is_none() {
        assert!(parse_if_match(&HeaderMap::new()).unwrap().is_none());
    }

    #[test]
    fn bare_strong_validator_parses() {
        let etag = fixture_etag();
        assert!(parse_if_match(&headers_with(&etag)).unwrap().is_some());
    }

    #[test]
    fn quoted_strong_validator_parses() {
        let etag = fixture_etag();
        let quoted = format!("\"{etag}\"");
        assert!(parse_if_match(&headers_with(&quoted)).unwrap().is_some());
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        let etag = fixture_etag();
        let padded = format!("  \"{etag}\"  ");
        assert!(parse_if_match(&headers_with(&padded)).unwrap().is_some());
    }

    #[test]
    fn weak_validator_is_rejected() {
        let etag = fixture_etag();
        let weak = format!("W/\"{etag}\"");
        assert!(parse_if_match(&headers_with(&weak)).is_err());
    }

    #[test]
    fn star_is_rejected() {
        assert!(parse_if_match(&headers_with("*")).is_err());
    }

    #[test]
    fn empty_value_is_rejected() {
        assert!(parse_if_match(&headers_with(" ")).is_err());
    }

    #[test]
    fn malformed_validator_is_rejected() {
        assert!(parse_if_match(&headers_with("\"not-a-valid-etag\"")).is_err());
    }
}
