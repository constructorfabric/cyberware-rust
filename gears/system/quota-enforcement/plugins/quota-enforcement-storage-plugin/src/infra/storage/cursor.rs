//! Keyset cursor of the Quota list: the last returned `UUIDv7`, base64url
//! without padding. It carries position only; tenant and PDP scope are
//! re-applied on every page, so a cursor never grants access.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use quota_enforcement_sdk::{PageRequest, QuotaId};
use uuid::Uuid;

use crate::domain::ports::StoreError;

/// Largest page the plugin serves; larger requests are clamped.
pub const MAX_PAGE_LIMIT: u32 = 500;

/// Largest number of explicit ids one filter may name.
pub const MAX_FILTER_IDS: usize = 500;

/// The page size actually served: the platform default for `0`, otherwise the
/// request clamped to [`MAX_PAGE_LIMIT`].
#[must_use]
pub const fn effective_limit(requested: u32) -> u32 {
    let wanted = if requested == 0 {
        PageRequest::DEFAULT_LIMIT
    } else {
        requested
    };
    if wanted > MAX_PAGE_LIMIT {
        MAX_PAGE_LIMIT
    } else {
        wanted
    }
}

/// The cursor that continues after `last`.
#[must_use]
pub fn encode(last: QuotaId) -> String {
    URL_SAFE_NO_PAD.encode(last.as_uuid().as_bytes())
}

/// The position a cursor names.
///
/// # Errors
///
/// [`StoreError::InvalidCursor`] when `cursor` is not 16 base64url bytes.
pub fn decode(cursor: &str) -> Result<QuotaId, StoreError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(cursor)
        .map_err(|_| StoreError::InvalidCursor)?;
    Uuid::from_slice(&bytes)
        .map(QuotaId::new)
        .map_err(|_| StoreError::InvalidCursor)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "cursor_tests.rs"]
mod cursor_tests;
