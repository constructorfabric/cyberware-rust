//! The two pieces of the history read that are decidable without a database.
//!
//! Everything else this module claims — the commit order, the walk that resumes
//! where the last page stopped, the actor the projection reports and the
//! intervals it attaches — is a statement about rows in two tables, and is
//! proved in `tests/sqlite_history.rs` against a real one. A page assembled from
//! a vector this file built would certify that the assembly compiles, which is
//! not what any of those claims say.
//!
//! What is worth isolating is the **token**, because it is the whole of
//! "opaque": a cursor that survives a round trip is the only thing standing
//! between D-125's contract and a client that reconstructs the walk's position
//! from a value it can read. The sub-second instant is the load-bearing part of
//! the case — a token that carried milliseconds would round-trip every whole
//! second perfectly and would silently re-serve the cursor's own row for every
//! instant finer than one.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

use uuid::Uuid;

use super::{HistoryPageRequest, decode, encode};
use crate::api::rest::cursor::{DEFAULT_LIMIT, MAX_LIMIT};
use crate::domain::error::DomainError;
use crate::domain::instant::from_unix;
use crate::infra::storage::repo::price_repo::HistoryPosition;
use time::OffsetDateTime;

/// An instant carrying sub-second digits, which is what makes the round trip a
/// test of anything.
fn instant() -> OffsetDateTime {
    from_unix(1_785_000_000, 123_456_789).expect("a representable instant")
}

fn position() -> HistoryPosition {
    HistoryPosition {
        authored_at: instant(),
        price_id: Uuid::from_u128(0x_b0_11_ce),
    }
}

#[test]
fn a_cursor_round_trips_whole_and_stays_opaque() {
    let token = encode(position());

    assert!(
        !token.contains(&position().price_id.to_string()),
        "a token carrying the key verbatim is a token a caller will construct: {token}"
    );
    let back = decode(&token).expect("the token this surface issued");
    assert_eq!(back, position());
    // Named separately: the id surviving while the instant is rounded is the
    // failure this token's two-field encoding exists to prevent, and an
    // assertion on the pair alone would not say which half moved.
    assert_eq!(back.authored_at.nanosecond(), 123_456_789);
}

#[test]
fn a_page_resumes_at_the_position_its_cursor_names() {
    let request = HistoryPageRequest::parse(None, Some(&encode(position())))
        .expect("a page resumes at its cursor");

    assert_eq!(request.after, Some(position()));
}

#[test]
fn an_undecodable_cursor_is_a_malformed_request_and_mints_no_code() {
    // Not base64; empty; the right encoding at the wrong length in both
    // directions - a 16-byte id token from the sibling surface, and 29 bytes.
    let too_long = URL_SAFE_NO_PAD.encode([0_u8; 29]);
    let sibling_token = URL_SAFE_NO_PAD.encode(Uuid::from_u128(0x_b0_11_ce).as_bytes());
    for raw in ["not-base64!!", "", &sibling_token, &too_long] {
        let err = decode(raw).expect_err("only a token this surface issued decodes");
        assert!(
            matches!(err, DomainError::InvalidRequest(_)),
            "{raw:?} -> {err:?}"
        );
        assert!(
            HistoryPageRequest::parse(None, Some(raw)).is_err(),
            "the request carrying it is refused too: {raw:?}"
        );
    }
}

#[test]
fn a_cursor_naming_an_unrepresentable_instant_is_refused() {
    // A well-formed token of the right length whose seconds field no instant
    // answers to. It is refused rather than clamped: a clamp would resume the
    // walk at an instant the caller never asked for, and would do it silently.
    let raw: Vec<u8> = i64::MAX
        .to_be_bytes()
        .into_iter()
        .chain(0_u32.to_be_bytes())
        .chain(*Uuid::from_u128(0x_b0_11_ce).as_bytes())
        .collect();
    let err = decode(&URL_SAFE_NO_PAD.encode(raw)).expect_err("no instant is at i64::MAX seconds");

    assert!(matches!(err, DomainError::InvalidRequest(_)), "{err:?}");
}

#[test]
fn the_interactive_default_is_a_hundred_and_the_cap_is_the_slo_unit() {
    // `inst-he-export` (C-6): a 1,000-row page is budgeted at 50s and is an
    // export shape, so an interactive read that names no size gets 100. Both
    // numbers are D-125's, are declared in `api::rest::cursor` and are pinned
    // there by `cursor_tests`; what this case pins is the half that is genuinely
    // this module's - that the history request reaches for those constants
    // rather than carrying a second opinion, and that its three branches pick
    // the same one of them the sibling surface does.
    assert_eq!(
        HistoryPageRequest::parse(None, None)
            .expect("no parameters is a valid first page")
            .limit,
        DEFAULT_LIMIT
    );
    assert_eq!(
        HistoryPageRequest::parse(Some(5_000), None)
            .expect("an oversized limit is served, capped")
            .limit,
        MAX_LIMIT
    );
    let err =
        HistoryPageRequest::parse(Some(0), None).expect_err("a page of no rows is not a page");
    assert!(matches!(err, DomainError::InvalidRequest(_)), "{err:?}");
}
