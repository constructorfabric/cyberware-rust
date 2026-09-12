//! The Auditor read over `pricing_audit_log` — `design/05-governance.md` §5's
//! `GET /bss-pricing/v1/audit`, `inst-au-read`, D-12, D-125.
//!
//! `infra/history.rs`'s twin one plane over, and deliberately its twin: a
//! commit-ordered keyset walk over an append-only store, a page request that
//! restates D-125's three branches over the two numbers D-125 owns, and an opaque
//! token that spells the walk's whole position. What differs is the store, the
//! audience and the position's width — and each difference is stated where it
//! lands rather than left for a reader to infer from a shared shape.
//!
//! # Why this exists at all, and what depended on it
//!
//! The table had **one** entry point — [`audit_repo::append`] — and no reader on
//! any mounted surface, while `infra/error_mapping.rs` justifies dropping the
//! detail from three 403 arms on the ground that *"the attempt itself is already
//! on `pricing_audit_log` as a `deny` record carrying that id … a durable trail
//! rather than a log line"*. The trail was durable and unreachable, so the
//! compensating control the error ladder names did not exist. This is that
//! control. The route is the one the design set names; nothing here invents a
//! surface.
//!
//! # Auditor-only, and the actor is the record's own column
//!
//! `audit × read` gates it (D-12), which is the same pair `/history` asks for and
//! **not** the same disclosure: that surface answers `pricing_price` rows with the
//! authoring principal off their own `created_by`, while this one answers the
//! before/after states and the approval trail. The permission is shared because
//! D-12 confines both to the Auditor; the content is not.
//!
//! Every principal id here is pseudonymous by the column's own type
//! (`inst-au-pii`), so a page carries no directly identifying operator PII and this
//! surface needs no redaction step. That is a property of the writer, asserted
//! there, and it is the reason a 7-year store can be read at all.
//!
//! # What is **not** here
//!
//! **Filters.** `inst-au-read` says "under Auditor-only filters" and enumerates
//! none, while the two discriminators a filter would name — `subject_kind` and
//! `action` — are declared vocabularies (D-158). A filter set chosen here would be
//! a wire contract this gear invented, so the page is unfiltered and the filters
//! are owed. A caller narrows by walking, which the cursor makes bounded.
//!
//! **The export.** `audit × export` is a second permission for a second shape —
//! chunks under their own p95, not pages — and nothing gates on it. Owed, and not
//! approximated by a large `limit`.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

use serde_json::Value as JsonValue;
use toolkit_db::secure::{AccessScope, DBRunner};
use toolkit_db::{DBProvider, DbError};
use toolkit_odata::ODataQuery;
use uuid::Uuid;

use crate::api::rest::cursor::{DEFAULT_LIMIT, MAX_LIMIT};
use crate::domain::error::DomainError;
use crate::domain::instant::from_unix;
use crate::infra::storage::RepoError;
use crate::infra::storage::entity::audit_log;
use crate::infra::storage::odata_mapping::OdataPageError;
use crate::infra::storage::repo::audit_repo::{self, AuditPosition};
use time::OffsetDateTime;

/// One page's worth of request against the audit read.
///
/// [`crate::infra::history::HistoryPageRequest`]'s shape, and the fields are
/// private for its stated reason: a `limit` of zero makes the walk report itself
/// exhausted at row 0 — the probe row is the only row fetched, it is popped, and
/// the caller is handed an empty page with no cursor, which is a silent end to a
/// trail that has not ended. [`Self::parse`] is the only door.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuditPageRequest {
    limit: u64,
    after: Option<AuditPosition>,
}

impl AuditPageRequest {
    /// Read a page request from D-125's two query parameters.
    ///
    /// The three branches are restated rather than delegated to
    /// [`crate::api::rest::cursor::PageRequest::parse`], for the reason its
    /// history twin gives in full: `DE0202` refuses an `api` DTO type named in
    /// `infra`, and what crosses instead is the pair of **numbers** D-125 owns.
    ///
    /// # Errors
    /// [`DomainError::InvalidRequest`] for `limit = 0` and for a cursor that does
    /// not decode. Neither mints a wire code: an undecodable cursor is a malformed
    /// request under the Foundation validation envelope.
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

    /// The clamped page size this request was parsed to.
    #[must_use]
    pub const fn limit(&self) -> u64 {
        self.limit
    }

    /// Where the previous page ended, when there was one.
    #[must_use]
    pub const fn after(&self) -> Option<AuditPosition> {
        self.after
    }
}

/// The opaque token naming the position a page left the walk at.
///
/// **36 bytes**: `i64` seconds, `u32` sub-second nanoseconds, the 16-byte
/// `chain_id`, `u64` `seq`. The instant is two fields for
/// [`crate::infra::history::encode`]'s reason — counting nanoseconds from the
/// epoch overflows an `i64` in 2262, and counting anything coarser truncates an
/// instant the store holds finer, so the truncated cursor sorts *before* the row
/// it names and the next page serves that row twice.
///
/// The two key fields after it are what the history token does not need: this
/// store's rows are not identified by one id. Dropping either would leave the
/// token naming a **set** of rows inside one instant, and a walk resuming after a
/// set either skips its tail or repeats it.
#[must_use]
pub fn encode(position: AuditPosition) -> String {
    let raw: Vec<u8> = position
        .recorded_at
        .unix_timestamp()
        .to_be_bytes()
        .into_iter()
        .chain(position.recorded_at.nanosecond().to_be_bytes())
        .chain(*position.chain_id.as_bytes())
        .chain(position.seq.to_be_bytes())
        .collect();
    URL_SAFE_NO_PAD.encode(raw)
}

/// The position an opaque cursor names.
///
/// # Errors
/// [`DomainError::InvalidRequest`] when the token is not URL-safe base64, is not
/// exactly the 36 bytes [`encode`] writes, or names an instant outside the range a
/// [`OffsetDateTime`] holds. One refusal for all of them, because a caller can act
/// no differently on any: the token did not come from this surface.
pub fn decode(raw: &str) -> Result<AuditPosition, DomainError> {
    let refuse = || {
        DomainError::InvalidRequest(
            "cursor: the token is not one this surface issued; \
             pass back a `next_cursor` verbatim, or omit it to start from the beginning"
                .to_owned(),
        )
    };
    let bytes = URL_SAFE_NO_PAD.decode(raw).map_err(|_| refuse())?;
    let (seconds, rest) = bytes.split_first_chunk::<8>().ok_or_else(refuse)?;
    let (nanos, rest) = rest.split_first_chunk::<4>().ok_or_else(refuse)?;
    let (chain, seq) = rest.split_first_chunk::<16>().ok_or_else(refuse)?;
    // Fixes the total length as well as the last field: a longer token leaves more
    // than eight bytes here and is refused.
    let seq: [u8; 8] = seq.try_into().map_err(|_| refuse())?;
    let recorded_at =
        from_unix(i64::from_be_bytes(*seconds), u32::from_be_bytes(*nanos)).ok_or_else(refuse)?;
    Ok(AuditPosition {
        recorded_at,
        chain_id: Uuid::from_bytes(*chain),
        seq: u64::from_be_bytes(seq),
    })
}

/// One audit record as a reader receives it.
///
/// An owned view rather than [`crate::domain::audit::AuditRecord`], which borrows
/// its subject and state and exists to be **hashed**: the preimage's shape is the
/// tamper-evidence contract and a reader that pinned it would make every rendering
/// choice a chain-format change.
///
/// `action` and `subject_kind` are carried as the column holds them and **not**
/// parsed into their vocabularies. Both are declared, additive sets (D-158), and
/// this is the forensic surface: a token the enum has not caught up with is
/// precisely the record an auditor most needs to see, and a reader that refused it
/// would answer 500 exactly when something unexpected had happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditEntry {
    /// The audited subject's aggregate — this record's chain segment (D-135).
    pub chain_id: Uuid,
    /// The record's position within that segment, `0` at genesis.
    ///
    /// Signed, as the column is, and for the same reason `action` is unparsed: a
    /// position this writer could not have produced is a record an auditor needs to
    /// be shown rather than a row a reader refuses. The **cursor** is where a
    /// negative `seq` is refused ([`AuditPosition::of`]), because there a wrong
    /// value silently moves the walk.
    pub seq: i64,
    /// `mutation` | `rollup`.
    pub entry_kind: String,
    /// When the mutation was recorded, UTC.
    pub recorded_at: OffsetDateTime,
    /// The **pseudonymous** principal who acted (`inst-au-pii`).
    pub actor_principal_id: Uuid,
    /// What was done, as the column holds it.
    pub action: String,
    /// What kind of thing it was done to.
    pub subject_kind: String,
    /// Which one.
    pub subject_ref: String,
    /// The subject's state before the mutation.
    pub before_state: Option<JsonValue>,
    /// The subject's state after it.
    pub after_state: Option<JsonValue>,
    /// The approval record the mutation ran under, when it had one — the trail
    /// `inst-au-complete` requires and the id the 403 arms drop.
    pub approval_ref: Option<Uuid>,
    /// The correlation id of the request that caused it (D-178).
    pub correlation_id: Option<Uuid>,
}

/// One page of the trail, and where the next one resumes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditPage {
    /// The page's records in commit order, oldest first.
    pub entries: Vec<AuditEntry>,
    /// The position to resume at, or `None` when the walk is exhausted.
    ///
    /// `None` on the last page rather than on the page after it, which is why the
    /// read asks the store for one row more than the page.
    pub next: Option<AuditPosition>,
}

/// `inst-au-read`'s Auditor read: the tenant's trail, one page at a time.
///
/// A reader with a provider, [`crate::infra::history::HistoryExporter`]'s shape —
/// and note the contrast with [`audit_repo`]'s CONTRACT, which forbids the
/// *writer* a provider of its own because the record must commit inside the
/// mutation's transaction. A read has no such obligation: there is no mutation to
/// be inside of.
#[derive(Clone)]
pub struct AuditReader {
    db: DBProvider<DbError>,
}

impl AuditReader {
    /// Build the reader over one database provider.
    #[must_use]
    pub const fn new(db: DBProvider<DbError>) -> Self {
        Self { db }
    }

    /// One page of the tenant's audit trail, oldest first.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure; [`RepoError::CorruptRow`]
    /// when a row's `seq` is not a position this chain can count in.
    pub async fn read_page(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        request: AuditPageRequest,
    ) -> Result<AuditPage, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        self.read_page_on(&conn, scope, tenant_id, request).await
    }

    /// The same read on a caller-owned runner.
    ///
    /// Split out so a suite can drive the walk inside one transaction, and so the
    /// probe-row arithmetic below has exactly one home.
    ///
    /// # Errors
    /// [`Self::read_page`]'s.
    pub async fn read_page_on(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        tenant_id: Uuid,
        request: AuditPageRequest,
    ) -> Result<AuditPage, RepoError> {
        // One row more than the page, so "is there another page" is answered
        // without a second query and without a `next` that points at nothing. The
        // store does not decide this: only the surface knows the page size it
        // promised.
        let probe = request.limit().saturating_add(1);
        let mut rows = audit_repo::page(runner, scope, tenant_id, probe, request.after()).await?;
        let has_more = u64::try_from(rows.len()).unwrap_or(u64::MAX) > request.limit();
        if has_more {
            rows.pop();
        }
        let next = match rows.last() {
            Some(row) if has_more => Some(AuditPosition::of(row)?),
            _ => None,
        };
        let entries = rows.into_iter().map(entry_of).collect();
        Ok(AuditPage { entries, next })
    }

    /// One `OData` page of the audit trail. Envelope mapping is the handler's.
    ///
    /// # Errors
    /// [`OdataPageError::Db`] on storage failure; [`OdataPageError::Odata`] on a
    /// malformed `$filter` / `$orderby` / cursor.
    pub async fn read_odata(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        query: &ODataQuery,
    ) -> Result<(Vec<AuditEntry>, Option<String>), OdataPageError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| OdataPageError::Db(format!("conn: {e}")))?;
        let page = audit_repo::list_odata(&conn, scope, tenant_id, query).await?;
        let entries = page.items.into_iter().map(entry_of).collect();
        Ok((entries, page.page_info.next_cursor))
    }
}

/// The read view of one stored row.
///
/// The hash columns are **not** carried. `prev_hash` and `row_hash` are the
/// verification job's inputs (`pricing_audit_chain_verified`, G4), and a reader
/// handed them would let a caller believe a page it re-hashed itself had been
/// verified — while the roll-up, which is what makes a deleted *segment*
/// detectable, is not on this surface at all. Tamper evidence is a job's answer,
/// not a field.
fn entry_of(row: audit_log::Model) -> AuditEntry {
    AuditEntry {
        chain_id: row.chain_id,
        seq: row.seq,
        entry_kind: row.entry_kind,
        recorded_at: row.recorded_at,
        actor_principal_id: row.actor_principal_id,
        action: row.action,
        subject_kind: row.subject_kind,
        subject_ref: row.subject_ref,
        before_state: row.before_state,
        after_state: row.after_state,
        approval_ref: row.approval_ref,
        correlation_id: row.correlation_id,
    }
}
