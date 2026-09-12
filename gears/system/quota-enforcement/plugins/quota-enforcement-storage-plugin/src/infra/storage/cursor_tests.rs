#![allow(clippy::expect_used)]

use quota_enforcement_sdk::{PageRequest, QuotaId};

use super::{MAX_PAGE_LIMIT, decode, effective_limit, encode};
use crate::domain::ports::StoreError;

#[test]
fn the_effective_limit_defaults_zero_and_clamps_large_requests() {
    assert_eq!(effective_limit(0), PageRequest::DEFAULT_LIMIT);
    assert_eq!(effective_limit(7), 7);
    assert_eq!(effective_limit(MAX_PAGE_LIMIT), MAX_PAGE_LIMIT);
    assert_eq!(effective_limit(10_000), MAX_PAGE_LIMIT);
}

#[test]
fn a_cursor_round_trips_its_position_and_carries_nothing_else() {
    let id = QuotaId::generate();
    let cursor = encode(id);
    assert_eq!(cursor.len(), 22, "16 bytes, base64url, no padding");
    assert!(!cursor.contains('='));
    assert_eq!(decode(&cursor).expect("decodes"), id);
}

#[test]
fn a_malformed_cursor_is_rejected() {
    for bad in ["", "not base64!", "AAAA", "AAAAAAAAAAAAAAAAAAAA"] {
        assert_eq!(decode(bad), Err(StoreError::InvalidCursor), "{bad:?}");
    }
}
