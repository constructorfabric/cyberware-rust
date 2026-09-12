//! D-125's collection contract, decided once for every list surface this gear
//! serves: `limit` (server default 100, hard cap 1,000) plus an **opaque**
//! `cursor`, over a stable key order, with `next_cursor` on every page until the
//! result is exhausted.
//!
//! # Why a keyset walk and not an offset
//!
//! D-125 forbids offset pagination outright, and the reason is the store rather
//! than taste: these tables are append-only over a >= 7-year retention, so
//! `OFFSET n` names a different row every time somebody inserts ahead of it. A
//! keyset walk names a **row**, so an insert ahead of the cursor cannot shift
//! what the next page starts at.
//!
//! # The stability guarantee, and exactly where it stops
//!
//! D-125 promises a walk that "never skips or duplicates a row at or before the
//! cursor". A keyset walk on `price_id ASC` gives that for **inserts**: a row
//! inserted ahead of the cursor is simply not visited (it sorts before the
//! walk's position), and one inserted behind is visited once.
//!
//! It does **not** give it for a **delete**. Draft price rows are deletable
//! (D-141), and a row deleted behind the cursor was already returned to the
//! caller, while one deleted ahead of it is never returned at all. Neither is a
//! skip or a duplicate of a row *at or before* the cursor — the letter of D-125
//! holds — but a caller that walks a page set and reassembles it does not get a
//! snapshot, and no cursor over a mutable draft plane can give one. This is a
//! **deliberately absent** guarantee, stated so a reader does not assume it, not
//! a defect to paper over: providing it needs an MVCC snapshot held across
//! pages, which is a transaction spanning HTTP requests.
//!
//! # `prev_cursor` is always `None`
//!
//! [`toolkit_odata::PageInfo`] carries the field because other surfaces on the
//! platform use it. D-125 specifies a **forward** walk only, and a backward
//! cursor over an append-only store is a guarantee nothing here can keep: the
//! rows behind a cursor are exactly the ones a concurrent delete may have
//! removed, so a backward page would be answering about a set that no longer
//! exists. Serving `null` says so; serving a token that sometimes works would
//! not.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

use toolkit_odata::PageInfo;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::instant::from_unix;
use time::OffsetDateTime;

/// Rows per page when the caller names no `limit` (D-125).
pub const DEFAULT_LIMIT: u64 = 100;

/// The hard cap on `limit` (D-125) — the unit the export SLO is expressed in.
pub const MAX_LIMIT: u64 = 1_000;

/// One page's worth of request: how many rows, and where to resume.
///
/// `after` is the **decoded** cursor, so nothing below this module ever sees the
/// encoding. That is what "opaque" has to mean to be worth anything: a client
/// that decoded the token and constructed its own would be depending on a
/// representation this module is free to change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageRequest {
    /// How many rows the page may carry, already clamped to [`MAX_LIMIT`].
    pub limit: u64,
    /// The key the previous page ended at; the walk resumes strictly after it.
    pub after: Option<Uuid>,
}

impl PageRequest {
    /// Read a page request from the two query parameters.
    ///
    /// **A `limit` above the cap is clamped, not refused.** The cap is a
    /// *server* limit — the page size the SLO is stated per — and a caller
    /// asking for more has made no mistake it could correct: refusing would
    /// force every client to know a number the server owns. A `limit` of
    /// **zero** is refused, because it is not a smaller page but a request for
    /// no rows at all, which no caller means and which would walk forever.
    ///
    /// # Errors
    /// [`DomainError::InvalidRequest`] for `limit = 0`, and for a cursor that
    /// does not decode. An undecodable cursor mints no code of its own — it is a
    /// malformed request under the Foundation validation envelope, the same
    /// reading D-141 gives an absent `If-Match`.
    pub fn parse(limit: Option<u64>, cursor: Option<&str>) -> Result<Self, DomainError> {
        let limit = match limit {
            None => DEFAULT_LIMIT,
            Some(0) => {
                return Err(DomainError::InvalidRequest(
                    "limit must be at least 1; a page of zero rows never advances".to_owned(),
                ));
            }
            Some(asked) => asked.min(MAX_LIMIT),
        };
        let after = cursor.map(decode).transpose()?;
        Ok(Self { limit, after })
    }
}

/// The `limit` query parameter, read out of its raw text.
///
/// # Why the query types carry `Option<String>` and this exists
///
/// A `Query<T>` member typed `Option<u64>` is parsed by **axum's** extractor, and
/// its rejection is a bare `400` with *no problem document at all* — against a
/// registration whose declared 400 has `Problem` as its schema. That is the same
/// defect class as a handler taking the `Json` extractor, one axis over:
/// `?limit=abc` on any of the nine paginated reads answered outside the canonical
/// envelope, so a client keying on the document got nothing to key on.
///
/// `windows.rs`'s `SellabilityQuery` recorded the lesson and applied it to one
/// struct of twelve. This is the shared half, so the nine paginated reads cannot
/// each phrase the refusal differently.
///
/// # Errors
/// [`DomainError::InvalidRequest`] naming the parameter, for a value that is not a
/// non-negative integer. No new wire code — a parameter this gear cannot interpret
/// is the Foundation validation envelope's own case, the same reading D-141 gives
/// an absent `If-Match` and [`decode`] gives an undecodable cursor.
pub fn parse_limit(raw: Option<&str>) -> Result<Option<u64>, DomainError> {
    raw.map(|value| {
        value.trim().parse::<u64>().map_err(|_| {
            DomainError::InvalidRequest(format!(
                "limit: `{value}` is not a whole number of rows; send a positive integer, or omit \
                 it for the server default of {DEFAULT_LIMIT}"
            ))
        })
    })
    .transpose()
}

/// A `Uuid`-valued query parameter, read out of its raw text.
///
/// [`parse_limit`]'s reason on the other type: `Option<Uuid>` at the query struct
/// hands the refusal to axum's extractor. `subject` names the parameter in the
/// refusal, because a caller told only "invalid argument" over a request carrying
/// two ids cannot act.
///
/// # Errors
/// [`DomainError::InvalidRequest`] naming the parameter and the value.
pub fn parse_uuid_param(subject: &str, raw: Option<&str>) -> Result<Option<Uuid>, DomainError> {
    raw.map(|value| {
        value
            .trim()
            .parse::<Uuid>()
            .map_err(|_| DomainError::InvalidRequest(format!("{subject}: `{value}` is not a UUID")))
    })
    .transpose()
}

/// The opaque token naming the last row of a page.
///
/// The encoding — URL-safe base64 without padding, over the key's 16 raw bytes —
/// is declared **here and nowhere else**. It is deliberately not the key's own
/// text form: a token a caller can read is a token a caller will construct, and
/// then the walk's contract is whatever that caller assumed rather than what
/// this module promises.
#[must_use]
pub fn encode(after: Uuid) -> String {
    URL_SAFE_NO_PAD.encode(after.as_bytes())
}

/// The key an opaque cursor names.
///
/// # Errors
/// [`DomainError::InvalidRequest`] when the token is not URL-safe base64, or
/// does not decode to exactly the 16 bytes of a key. No new wire code: an
/// undecodable cursor is a request this gear cannot interpret, which is the
/// Foundation validation envelope's own case.
pub fn decode(raw: &str) -> Result<Uuid, DomainError> {
    let refuse = || {
        DomainError::InvalidRequest(
            "cursor: the token is not one this surface issued; \
             pass back a `next_cursor` verbatim, or omit it to start from the beginning"
                .to_owned(),
        )
    };
    let bytes = URL_SAFE_NO_PAD.decode(raw).map_err(|_| refuse())?;
    Uuid::from_slice(&bytes).map_err(|_| refuse())
}

/// One page's worth of request over a walk keyed by an **instant and an id**.
///
/// [`PageRequest`]'s shape for the surfaces whose stable order is not a single
/// column. `pricing_group_membership` is the case D-322 clause 4 names: its rows
/// are effective-dated, and the auditor the clause names reads them in
/// `(effective_from, membership_id)` order, which no lone `membership_id` cursor
/// can resume from. A walk whose sort key and resume key disagree skips or repeats
/// rows — so the two move together or not at all, which is why this is a type and
/// not an extra argument.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IntervalPageRequest {
    /// How many rows the page may carry, already clamped to [`MAX_LIMIT`].
    pub limit: u64,
    /// The `(instant, id)` pair the previous page ended at; the walk resumes
    /// strictly after it.
    pub after: Option<(OffsetDateTime, Uuid)>,
}

impl IntervalPageRequest {
    /// Read a page request from the two query parameters.
    ///
    /// The three `limit` branches are [`PageRequest::parse`]'s, against the same
    /// two constants — clamped above the cap, refused at zero, defaulted when
    /// absent — because D-125 states them once for every list surface.
    ///
    /// # Errors
    /// [`DomainError::InvalidRequest`] for `limit = 0`, and for a cursor that does
    /// not decode. See [`decode_instant_and_id`] for why an undecodable token
    /// mints no wire code of its own.
    pub fn parse(limit: Option<u64>, cursor: Option<&str>) -> Result<Self, DomainError> {
        let limit = match limit {
            None => DEFAULT_LIMIT,
            Some(0) => {
                return Err(DomainError::InvalidRequest(
                    "limit must be at least 1; a page of zero rows never advances".to_owned(),
                ));
            }
            Some(asked) => asked.min(MAX_LIMIT),
        };
        let after = cursor.map(decode_instant_and_id).transpose()?;
        Ok(Self { limit, after })
    }
}

/// The opaque token naming a position that is an **instant and an id**.
///
/// # The instant is two fields, and that is not padding
///
/// 28 bytes: `i64` seconds, `u32` sub-second nanoseconds, the id's 16 bytes. A
/// **single** count of either unit would have been shorter and wrong. Counting
/// nanoseconds from the epoch overflows an `i64` in 2262, which a >= 7-year
/// retention has no business depending on; counting milliseconds or microseconds
/// silently **truncates** an instant the store holds at finer resolution, and a
/// truncated cursor sorts *before* the row it names — so the next page would open
/// by serving that row a second time. The pair round-trips every value a
/// [`OffsetDateTime`] can hold, exactly.
///
/// # One encoder, two callers, and why that needed saying
///
/// `infra::history`'s own cursor is this same 28-byte shape, and it reached these
/// conclusions first — the paragraph above is its argument, moved here rather than
/// restated. `history::encode`/`decode` now delegate, so the byte layout has one
/// definition: a second hand-maintained copy is how a walk comes to issue tokens
/// its own reader cannot resume from, and this crate has paid for that class in the
/// unit-comparison list already (D-127).
///
/// The layering is why the shared half lives **here** and the typed half stays
/// there: `DE0202` refuses an `api` DTO *type* named in `infra`, so
/// [`crate::infra::history::HistoryPosition`] cannot move into this module and
/// this module's request types cannot move into that one. What crosses is a pair of
/// primitives — the same reading under which `history` already takes
/// [`DEFAULT_LIMIT`] and [`MAX_LIMIT`] from here.
#[must_use]
pub fn encode_instant_and_id(at: OffsetDateTime, id: Uuid) -> String {
    let raw: Vec<u8> = at
        .unix_timestamp()
        .to_be_bytes()
        .into_iter()
        .chain(at.nanosecond().to_be_bytes())
        .chain(*id.as_bytes())
        .collect();
    URL_SAFE_NO_PAD.encode(raw)
}

/// The `(instant, id)` pair an opaque cursor names.
///
/// # Errors
/// [`DomainError::InvalidRequest`] when the token is not URL-safe base64, is not
/// exactly the 28 bytes [`encode_instant_and_id`] writes, or names an instant
/// outside the range a [`OffsetDateTime`] holds. All three are one refusal, because
/// a caller can act no differently on any of them: the token did not come from this
/// surface. No new wire code, for [`decode`]'s reason.
pub fn decode_instant_and_id(raw: &str) -> Result<(OffsetDateTime, Uuid), DomainError> {
    let refuse = || {
        DomainError::InvalidRequest(
            "cursor: the token is not one this surface issued; \
             pass back a `next_cursor` verbatim, or omit it to start from the beginning"
                .to_owned(),
        )
    };
    let bytes = URL_SAFE_NO_PAD.decode(raw).map_err(|_| refuse())?;
    let (seconds, rest) = bytes.split_first_chunk::<8>().ok_or_else(refuse)?;
    let (nanos, id) = rest.split_first_chunk::<4>().ok_or_else(refuse)?;
    // Fixes the total length as well as the last field: a longer token leaves more
    // than sixteen bytes here and is refused.
    let id: [u8; 16] = id.try_into().map_err(|_| refuse())?;
    let at =
        from_unix(i64::from_be_bytes(*seconds), u32::from_be_bytes(*nanos)).ok_or_else(refuse)?;
    Ok((at, Uuid::from_bytes(id)))
}

/// A cursor over `(price_overlay_id, revision)` — 24 bytes: the id's 16, then the
/// revision as a big-endian `u64`.
///
/// **A second pair rather than a reuse of the 16-byte token**. The
/// overlay list is ordered `(price_overlay_id, revision)` and was resumed on the
/// id alone, so a page boundary falling between two revisions of one overlay
/// dropped the rest of them — against a registration whose own description says
/// *"Returns every revision"* and *"a keyset walk has to be ordered by the key its
/// cursor names"*. The sort key and the resume key have to be the same tuple; the
/// alternative, sorting by the id alone, loses the ordering the walk needs within
/// an overlay.
#[must_use]
pub fn encode_id_and_revision(id: Uuid, revision: u64) -> String {
    let raw: Vec<u8> = id
        .as_bytes()
        .iter()
        .copied()
        .chain(revision.to_be_bytes())
        .collect();
    URL_SAFE_NO_PAD.encode(raw)
}

/// The `(id, revision)` pair an opaque cursor names.
///
/// # Errors
/// [`DomainError::InvalidRequest`] when the token is not URL-safe base64 or is not
/// exactly the 24 bytes [`encode_id_and_revision`] writes. One refusal for both,
/// for [`decode_instant_and_id`]'s reason: the token did not come from this
/// surface, and a caller can act no differently on the two cases.
pub fn decode_id_and_revision(raw: &str) -> Result<(Uuid, u64), DomainError> {
    let refuse = || {
        DomainError::InvalidRequest(
            "cursor: the token is not one this surface issued; \
             pass back a `next_cursor` verbatim, or omit it to start from the beginning"
                .to_owned(),
        )
    };
    let bytes = URL_SAFE_NO_PAD.decode(raw).map_err(|_| refuse())?;
    let (id, revision) = bytes.split_first_chunk::<16>().ok_or_else(refuse)?;
    // Fixes the total length as well as the last field.
    let revision: [u8; 8] = revision.try_into().map_err(|_| refuse())?;
    Ok((Uuid::from_bytes(*id), u64::from_be_bytes(revision)))
}

/// The page envelope's cursor block for an [`encode_id_and_revision`] walk.
#[must_use]
pub fn revision_page_info(next: Option<(Uuid, u64)>, limit: u64) -> PageInfo {
    PageInfo {
        next_cursor: next.map(|(id, revision)| encode_id_and_revision(id, revision)),
        prev_cursor: None,
        limit,
    }
}

/// The page envelope's cursor block for an [`IntervalPageRequest`]'s walk.
///
/// [`page_info`]'s reading, over the pair: `next` is `Some` only while the walk can
/// continue.
#[must_use]
pub fn interval_page_info(next: Option<(OffsetDateTime, Uuid)>, limit: u64) -> PageInfo {
    PageInfo {
        next_cursor: next.map(|(at, id)| encode_instant_and_id(at, id)),
        prev_cursor: None,
        limit,
    }
}

/// The page envelope's cursor block.
///
/// `next` is `Some` only while the walk can continue: a page that exhausted the
/// result carries `null`, which is what lets a client stop **without** issuing
/// the extra request that returns an empty page. See the module doc for why
/// `prev_cursor` is always `null`.
#[must_use]
pub fn page_info(next: Option<Uuid>, limit: u64) -> PageInfo {
    PageInfo {
        next_cursor: next.map(encode),
        prev_cursor: None,
        limit,
    }
}

#[cfg(test)]
#[path = "cursor_tests.rs"]
mod cursor_tests;
