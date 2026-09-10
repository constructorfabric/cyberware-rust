//! D-125's contract, asserted where it is decided.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use uuid::Uuid;

use super::{DEFAULT_LIMIT, MAX_LIMIT, PageRequest, decode, encode, page_info};
use crate::domain::error::DomainError;
use crate::domain::instant::from_unix;

/// **D-125's two numbers, pinned to their numbers.**
///
/// Every assertion in this file compared against `DEFAULT_LIMIT` and `MAX_LIMIT`
/// imported from the module under test, so changing either constant kept the whole
/// file green while breaking the decision it implements. The one place in the
/// eleven module suites that pins a spec constant to a literal is
/// `threshold_policy_tests`' `10001`, and it is right; this is the same discipline.
#[test]
fn the_page_numbers_are_the_ones_the_decision_names() {
    assert_eq!(DEFAULT_LIMIT, 100, "D-125's server default");
    assert_eq!(
        MAX_LIMIT, 1_000,
        "D-125's hard cap, the unit the export SLO is in"
    );
}

#[test]
fn an_absent_limit_takes_the_server_default() {
    let page = PageRequest::parse(None, None).expect("no parameters is a valid first page");

    assert_eq!(
        page.limit, 100,
        "the literal, not the constant it came from"
    );
    assert_eq!(page.after, None);
}

#[test]
fn a_limit_above_the_cap_is_clamped_rather_than_refused() {
    // The cap is a server limit, not a caller mistake: refusing would make every
    // client responsible for knowing a number the server owns.
    let page = PageRequest::parse(Some(5_000), None).expect("an oversized limit is served, capped");

    assert_eq!(
        page.limit, 1_000,
        "the literal, not the constant it came from"
    );
}

#[test]
fn a_zero_limit_is_refused_because_it_never_advances() {
    let err = PageRequest::parse(Some(0), None).expect_err("a page of no rows is not a page");

    assert!(matches!(err, DomainError::InvalidRequest(_)), "{err:?}");
}

#[test]
fn a_cursor_round_trips_and_stays_opaque() {
    let key = Uuid::now_v7();
    let token = encode(key);

    // **Opacity, asserted so it can fail.** This read `!token.contains(&key.to_string())`
    // until 2026-08-18, which no implementation can violate: the token is URL-safe
    // base64 of 16 raw bytes, 22 characters, and a uuid's text form is 36 — a
    // shorter string cannot contain a longer one, for any encoding including a
    // broken one. What opacity actually means here is that the token is *not* the
    // key's own text form, so both halves are asserted: the length the encoding
    // fixes, and the refusal of the text form a caller would otherwise construct.
    assert_eq!(token.len(), 22, "URL-safe base64 of 16 raw bytes: {token}");
    assert!(
        decode(&key.to_string()).is_err(),
        "the key's own text form is not a token this surface issued, or a caller who \
         skipped the cursor entirely would page successfully"
    );
    assert_eq!(decode(&token).expect("the token this surface issued"), key);
    assert_eq!(
        PageRequest::parse(None, Some(&token))
            .expect("a page resumes at its cursor")
            .after,
        Some(key)
    );
}

#[test]
fn an_undecodable_cursor_is_a_malformed_request_and_mints_no_code() {
    for raw in ["not-base64!!", "", "aGVsbG8"] {
        let err = decode(raw).expect_err("only a token this surface issued decodes");
        assert!(
            matches!(err, DomainError::InvalidRequest(_)),
            "{raw:?} -> {err:?}"
        );
    }
}

#[test]
fn the_envelope_says_null_forward_when_the_result_is_exhausted() {
    // A client must be able to stop WITHOUT issuing the extra request that
    // returns an empty page - which is what `next_cursor` on every page until
    // exhaustion means (D-125).
    let last = page_info(None, DEFAULT_LIMIT);
    assert!(last.next_cursor.is_none());
    assert_eq!(last.limit, DEFAULT_LIMIT);

    let more = page_info(Some(Uuid::now_v7()), 10);
    assert!(more.next_cursor.is_some());

    // Backwards is not offered, and the field says so rather than carrying a
    // token that sometimes works.
    assert!(last.prev_cursor.is_none());
    assert!(more.prev_cursor.is_none());
}

// ---------------------------------------------------------------------------
// The interval-keyed walk (D-322 clause 4).
// ---------------------------------------------------------------------------

/// The pair round-trips **including its sub-second part**.
///
/// The precision is the whole reason the token carries two fields: a cursor
/// truncated to milliseconds or microseconds sorts *before* the row it names, and
/// the next page opens by serving that row a second time. So the assertion is on an
/// instant whose nanoseconds are not a whole microsecond.
#[test]
fn an_interval_cursor_round_trips_to_the_nanosecond() {
    let at = from_unix(1_785_000_000, 123_456_789).expect("a representable instant");
    let id = Uuid::from_u128(0x_c0_de);

    let token = super::encode_instant_and_id(at, id);
    let (back_at, back_id) = super::decode_instant_and_id(&token).expect("our own token decodes");

    assert_eq!(back_at, at, "the instant survives whole");
    assert_eq!(back_at.nanosecond(), 123_456_789);
    assert_eq!(back_id, id);
    // **The encoder, pinned by its bytes**, as the other two shapes are — one by a
    // length and one by a literal. A round trip constrains the pair `encode`/`decode`
    // and neither alone, and a refusal of a caller-constructed key constrains the
    // *decoder*: under both, an encoder emitting `format!("{base64}|{id}")` stays
    // green while every cursor spells its key in plaintext.
    assert_eq!(
        token, "AAAAAGpk8EAHW80VAAAAAAAAAAAAAAAAAADA3g",
        "the 28-byte layout is the contract: 8 bytes of seconds, 4 of nanos, 16 of uuid"
    );
    assert_eq!(token.len(), 38, "URL-safe base64 of 28 raw bytes, unpadded");
    // And the text form a caller would otherwise construct is not a token this
    // surface issued. `-` is in the URL-safe alphabet, so a `!contains` would be a
    // claim about this fixture rather than about the encoding.
    assert!(
        super::decode_instant_and_id(&id.to_string()).is_err(),
        "a caller-constructed key is not a token this surface issued"
    );
}

/// **The two token shapes do not read each other's tokens.**
///
/// Two walks now issue cursors from this module — a 16-byte id and a 28-byte
/// `(instant, id)` pair — and the failure mode of two shapes on one contract is one
/// surface silently accepting the other's token and resuming somewhere arbitrary.
/// Each decoder is pinned to its own length, so the answer is the ordinary
/// "not one this surface issued" refusal in both directions.
#[test]
fn neither_cursor_shape_accepts_the_others_token() {
    let at = from_unix(1_785_000_000, 0).expect("a representable instant");
    let id = Uuid::from_u128(0x_c0_de);

    let interval_token = super::encode_instant_and_id(at, id);
    let plain_token = encode(id);

    assert!(
        matches!(decode(&interval_token), Err(DomainError::InvalidRequest(_))),
        "the id walk must refuse a 28-byte interval token"
    );
    assert!(
        matches!(
            super::decode_instant_and_id(&plain_token),
            Err(DomainError::InvalidRequest(_))
        ),
        "and the interval walk must refuse a 16-byte id token"
    );
}

/// The interval request reads the same three `limit` branches D-125 states once.
///
/// Restated rather than delegated, so this is the test that would catch them
/// drifting: a second page-request type is exactly where a cap or a default comes to
/// differ between two surfaces of one contract.
#[test]
fn the_interval_request_takes_the_same_limit_rules() {
    let default = super::IntervalPageRequest::parse(None, None).expect("an absent limit defaults");
    assert_eq!(default.limit, DEFAULT_LIMIT);
    assert_eq!(default.after, None);

    let clamped = super::IntervalPageRequest::parse(Some(MAX_LIMIT + 1), None).expect("clamped");
    assert_eq!(clamped.limit, MAX_LIMIT);

    assert!(
        matches!(
            super::IntervalPageRequest::parse(Some(0), None),
            Err(DomainError::InvalidRequest(_))
        ),
        "a page of zero rows never advances and is refused here too"
    );

    let at = from_unix(1_785_000_000, 7).expect("a representable instant");
    let id = Uuid::from_u128(0x_beef);
    let resumed =
        super::IntervalPageRequest::parse(Some(5), Some(&super::encode_instant_and_id(at, id)))
            .expect("its own token");
    assert_eq!(resumed.after, Some((at, id)));
}

/// The envelope says `null` forward when the interval walk is exhausted.
///
/// [`page_info`]'s property on the pair: a page that hands back a token the client
/// cannot use is how a walk comes to issue one extra empty request per collection.
#[test]
fn the_interval_envelope_says_null_forward_when_exhausted() {
    let exhausted = super::interval_page_info(None, 10);
    assert_eq!(exhausted.next_cursor, None);
    assert_eq!(exhausted.prev_cursor, None);
    assert_eq!(exhausted.limit, 10);

    let at = from_unix(1_785_000_000, 0).expect("a representable instant");
    let more = super::interval_page_info(Some((at, Uuid::from_u128(1))), 10);
    assert_eq!(
        more.next_cursor,
        Some(super::encode_instant_and_id(at, Uuid::from_u128(1))),
        "and names where to resume while it can"
    );
}

// ---------------------------------------------------------------------------
// The `(id, revision)` walk (review R-3).
// ---------------------------------------------------------------------------

/// The `(id, revision)` pair round-trips, **against a golden vector**.
///
/// The 24-byte shape exists because the overlay list is ordered
/// `(price_overlay_id, revision)` and was resumed on the id alone, so a page
/// boundary falling between two revisions of one overlay dropped the rest of them.
/// A round trip alone would hold for any layout the two halves agreed on — including
/// a little-endian revision — and what a cursor promises is that a token issued by
/// yesterday's deployment resumes on today's. So the byte layout is pinned to a
/// literal, the way `a_cursor_round_trips_and_stays_opaque` pins its 22 characters:
/// sixteen bytes of id, then the revision as a **big-endian** `u64`, URL-safe base64
/// without padding.
#[test]
fn a_revision_cursor_round_trips_against_its_byte_layout() {
    let id = Uuid::from_u128(0x0a1b_2c3d_4e5f_6071_8293_a4b5_c6d7_e8f9);
    let token = super::encode_id_and_revision(id, 258);

    // 0x0102 = 258, high byte first: a little-endian revision reads `AgEAAAAAAAAA`
    // in the tail and is a different token for the same pair.
    assert_eq!(
        token, "ChssPU5fYHGCk6S1xtfo-QAAAAAAAAEC",
        "the 24-byte layout is the contract, not an implementation detail"
    );
    assert_eq!(
        token.len(),
        32,
        "URL-safe base64 of 24 raw bytes, and the length is what pins the shape"
    );
    // The token is 32 characters and a uuid's text form is 36, so a `!contains`
    // here cannot fail for any implementation. Opacity means the token is not the
    // pair's own text form, and that is what this asserts.
    assert!(
        super::decode_id_and_revision(&format!("{id}-258")).is_err(),
        "a caller-constructed `{{id}}-{{revision}}` is not a token this surface issued"
    );

    let (back_id, back_revision) =
        super::decode_id_and_revision(&token).expect("our own token decodes");
    assert_eq!(back_id, id);
    assert_eq!(back_revision, 258);

    // The boundary values, because the tail is where a truncating encoding shows:
    // revision 0 is the first revision of every overlay, and `u64::MAX` is the
    // widest the column can carry.
    for revision in [0, 1, u64::MAX] {
        let token = super::encode_id_and_revision(id, revision);
        assert_eq!(
            super::decode_id_and_revision(&token).expect("our own token decodes"),
            (id, revision),
            "revision {revision} must survive the round trip"
        );
    }
}

/// An undecodable revision cursor is a malformed request and mints no code.
#[test]
fn an_undecodable_revision_cursor_is_a_malformed_request() {
    for raw in ["not-base64!!", "", "aGVsbG8"] {
        let err = super::decode_id_and_revision(raw)
            .expect_err("only a token this surface issued decodes");
        assert!(
            matches!(err, DomainError::InvalidRequest(_)),
            "{raw:?} -> {err:?}"
        );
    }
}

/// **The third token shape does not read the other two's, and neither reads it.**
///
/// [`neither_cursor_shape_accepts_the_others_token`]'s property, extended to the pair
/// R-3 added. Three shapes on one contract is where one surface comes to accept
/// another's token and resume somewhere arbitrary; each decoder is pinned to its own
/// length, so all four directions are the ordinary "not one this surface issued"
/// refusal.
#[test]
fn the_revision_cursor_and_the_other_two_shapes_refuse_each_other() {
    let at = from_unix(1_785_000_000, 0).expect("a representable instant");
    let id = Uuid::from_u128(0x_c0_de);

    let plain_token = encode(id);
    let interval_token = super::encode_instant_and_id(at, id);
    let revision_token = super::encode_id_and_revision(id, 7);

    // The 24-byte token is refused by the other two walks...
    assert!(
        matches!(decode(&revision_token), Err(DomainError::InvalidRequest(_))),
        "the id walk must refuse a 24-byte revision token"
    );
    assert!(
        matches!(
            super::decode_instant_and_id(&revision_token),
            Err(DomainError::InvalidRequest(_))
        ),
        "the interval walk must refuse a 24-byte revision token"
    );
    // ...and refuses theirs. The 16-byte case is the one that matters: it is the
    // token this walk itself issued before R-3, so accepting it would resume a
    // revision walk from a position naming no revision - the very truncation the
    // second column was added to end.
    assert!(
        matches!(
            super::decode_id_and_revision(&plain_token),
            Err(DomainError::InvalidRequest(_))
        ),
        "the revision walk must refuse a 16-byte id token"
    );
    assert!(
        matches!(
            super::decode_id_and_revision(&interval_token),
            Err(DomainError::InvalidRequest(_))
        ),
        "the revision walk must refuse a 28-byte interval token"
    );
}

/// The envelope says `null` forward when the revision walk is exhausted.
#[test]
fn the_revision_envelope_says_null_forward_when_exhausted() {
    let exhausted = super::revision_page_info(None, 10);
    assert_eq!(exhausted.next_cursor, None);
    assert_eq!(exhausted.prev_cursor, None);
    assert_eq!(exhausted.limit, 10);

    let id = Uuid::from_u128(2);
    let more = super::revision_page_info(Some((id, 4)), 10);
    assert_eq!(
        more.next_cursor,
        Some(super::encode_id_and_revision(id, 4)),
        "and names the (id, revision) pair to resume from while it can"
    );
    assert_eq!(more.prev_cursor, None);
}
