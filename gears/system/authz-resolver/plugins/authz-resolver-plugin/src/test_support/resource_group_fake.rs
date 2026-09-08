//! Scriptable in-memory `ResourceGroupReadHierarchy` fake.
//!
//! Mirrors the RBAC and tenant-resolver fakes' shape.
//!
//! `list_memberships` honors a minimal `OData` filter: `group_id in (uuid1, uuid2, ...)`.
//! Absent or unrecognized filter → return every configured membership. That is
//! the only shape the current tests need.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use resource_group_sdk::api::ResourceGroupReadHierarchy;
use resource_group_sdk::models::{ResourceGroup, ResourceGroupMembership, ResourceGroupWithDepth};
use toolkit_canonical_errors::{CanonicalError, resource_error};
use toolkit_odata::ast::{Expr, Value};
use toolkit_odata::page::{Page, PageInfo};
use toolkit_odata::{CursorV1, ODataQuery, SortDir};
use toolkit_security::SecurityContext;
use uuid::Uuid;

/// Group-scoped resource marker for building canonical errors in the fake
/// (mirrors the impl crate's `#[resource_error]` scope; the literal MUST equal
/// `resource_group_sdk::GROUP_RESOURCE_TYPE`).
#[resource_error("gts.cf.core.resource_group.group.v1~")]
struct RgScope;

/// Which paginated read misbehaves under [`InMemoryResourceGroupClient::set_stuck_cursor`].
///
/// Separate from the mode because the two drains are reached in sequence — the
/// descendants walk runs before the membership walk — so a single global switch
/// could only ever exercise the first of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StuckTarget {
    /// `get_group_descendants`.
    Descendants,
    /// `list_memberships`.
    Memberships,
}

/// How a scripted resolver fails to make progress while paging.
///
/// Both shapes are ones a correct resolver never produces and the well-behaved
/// [`paginate`] path cannot express: it emits a cursor only when items remain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StuckCursor {
    /// Every page is empty and carries the *same* cursor. No progress is made
    /// and the repetition is visible on the second page.
    RepeatedCursor,
    /// Every page is empty and carries a *fresh* cursor. Nothing repeats and the
    /// accumulated-item count never grows, so only a page budget can end it.
    EndlessEmptyPages,
}

#[derive(Debug, Clone, Default)]
struct Script {
    groups: HashMap<Uuid, ResourceGroup>,
    descendants: HashMap<Uuid, Vec<ResourceGroupWithDepth>>,
    memberships: Vec<ResourceGroupMembership>,
    error: Option<String>,
    /// When `Some(n)`, `get_group_descendants`/`list_memberships` return at most
    /// `n` items per page and a real cursor for the remainder — so tests can
    /// exercise the client's multi-page draining. `None` → single page.
    page_size: Option<usize>,
    /// When `Some`, the named read ignores its item set and answers with a page
    /// that always carries a cursor — the shape a misbehaving resolver produces.
    /// Used to prove the drain loops fail closed instead of spinning.
    stuck_cursor: Option<(StuckTarget, StuckCursor)>,
}

pub struct InMemoryResourceGroupClient {
    script: Mutex<Script>,
    call_count: AtomicUsize,
}

impl Default for InMemoryResourceGroupClient {
    fn default() -> Self {
        Self {
            script: Mutex::new(Script::default()),
            call_count: AtomicUsize::new(0),
        }
    }
}

impl InMemoryResourceGroupClient {
    /// Pre-populate the fake with groups (matched by id in `get_group`).
    #[must_use]
    pub fn with_groups(groups: Vec<ResourceGroup>) -> Self {
        let fake = Self::default();
        fake.add_groups(groups);
        fake
    }

    pub fn add_groups(&self, groups: Vec<ResourceGroup>) {
        let mut script = match self.script.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        for g in groups {
            script.groups.insert(g.id, g);
        }
    }

    /// Configure `get_group_descendants(group_id, _)` to return the
    /// supplied descendants page.
    #[must_use]
    pub fn with_group_descendants(
        group_id: Uuid,
        descendants: Vec<ResourceGroupWithDepth>,
    ) -> Self {
        let fake = Self::default();
        fake.add_group_descendants(group_id, descendants);
        fake
    }

    pub fn add_group_descendants(&self, group_id: Uuid, descendants: Vec<ResourceGroupWithDepth>) {
        let mut script = match self.script.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        script.descendants.insert(group_id, descendants);
    }

    /// Pre-populate the fake's membership list. `list_memberships` returns
    /// the entries that match a `group_id in (...)` filter (or every entry
    /// when no such filter is present).
    #[must_use]
    pub fn with_memberships(memberships: Vec<ResourceGroupMembership>) -> Self {
        let fake = Self::default();
        fake.add_memberships(memberships);
        fake
    }

    pub fn add_memberships(&self, memberships: Vec<ResourceGroupMembership>) {
        let mut script = match self.script.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        script.memberships.extend(memberships);
    }

    /// Make every method fail with `CanonicalError::Internal(message)`.
    #[must_use]
    pub fn with_error(message: impl Into<String>) -> Self {
        let fake = Self::default();
        fake.set_error(message);
        fake
    }

    pub fn set_error(&self, message: impl Into<String>) {
        let mut script = match self.script.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        script.error = Some(message.into());
    }

    pub fn clear_error(&self) {
        let mut script = match self.script.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        script.error = None;
    }

    /// Split `get_group_descendants`/`list_memberships` responses into pages of
    /// `n` items each (with real cursors), so tests can verify the client
    /// drains every page instead of reading only the first.
    pub fn set_page_size(&self, n: usize) {
        let mut script = match self.script.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        script.page_size = Some(n);
    }

    /// Make `target` answer with a page that never ends the walk, so a test can
    /// assert the drain fails closed rather than spinning forever.
    pub fn set_stuck_cursor(&self, target: StuckTarget, mode: StuckCursor) {
        let mut script = match self.script.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        script.stuck_cursor = Some((target, mode));
    }

    #[must_use]
    pub fn call_count(&self) -> usize {
        self.call_count.load(Ordering::SeqCst)
    }

    fn maybe_error(&self) -> Option<CanonicalError> {
        let script = match self.script.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        script.error.as_ref().map(|m| {
            CanonicalError::service_unavailable()
                .with_detail(m.clone())
                .create()
        })
    }
}

/// Extract the `Uuid` set from a `group_id in (...)` `OData` filter, if the
/// query carries such a filter. Returns `None` if no recognizable filter
/// is present — caller treats that as "no restriction, return everything".
fn extract_group_id_in_filter(query: &ODataQuery) -> Option<Vec<Uuid>> {
    let filter = query.filter()?;
    walk_for_group_id_in(filter)
}

fn walk_for_group_id_in(expr: &Expr) -> Option<Vec<Uuid>> {
    match expr {
        Expr::In(ident, values) => {
            if let Expr::Identifier(name) = ident.as_ref()
                && name == "group_id"
            {
                let mut ids = Vec::with_capacity(values.len());
                for v in values {
                    if let Expr::Value(Value::Uuid(uuid)) = v {
                        ids.push(*uuid);
                    } else {
                        return None;
                    }
                }
                return Some(ids);
            }
            None
        }
        Expr::And(l, r) | Expr::Or(l, r) => {
            walk_for_group_id_in(l).or_else(|| walk_for_group_id_in(r))
        }
        _ => None,
    }
}

fn empty_page<T>(items: Vec<T>) -> Page<T> {
    Page {
        items,
        page_info: PageInfo {
            next_cursor: None,
            prev_cursor: None,
            limit: 0,
        },
    }
}

/// A page shaped like a misbehaving resolver's: no items, cursor always present.
///
/// `RepeatedCursor` pins `k[0]` so every page hands back the token it was given;
/// `EndlessEmptyPages` increments it, so the tokens differ forever while the
/// item set stays empty.
fn stuck_page<T>(mode: StuckCursor, query: &ODataQuery) -> Page<T> {
    let offset = query
        .cursor
        .as_ref()
        .and_then(|c| c.k.first())
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0);
    let token = match mode {
        StuckCursor::RepeatedCursor => 0,
        StuckCursor::EndlessEmptyPages => offset + 1,
    };
    let cur = CursorV1 {
        k: vec![token.to_string()],
        o: SortDir::Asc,
        s: "+id".to_owned(),
        f: None,
        d: "fwd".to_owned(),
    };
    Page {
        items: Vec::new(),
        page_info: PageInfo {
            // `encode` of this fixed struct cannot fail; `.ok()` keeps the fake
            // free of `expect`/`unwrap`. A `None` would simply end the walk,
            // which is the opposite of what this helper is for.
            next_cursor: cur.encode().ok(),
            prev_cursor: None,
            limit: 0,
        },
    }
}

/// Page a full item set per `page_size`. The offset for the next page is stashed
/// in the cursor's `k[0]` (a real, round-trippable `CursorV1`), mirroring how a
/// keyset cursor carries resume state. With `page_size = None` this is a single
/// page (cursor `None`) — identical to [`empty_page`].
fn paginate<T: Clone>(items: Vec<T>, page_size: Option<usize>, query: &ODataQuery) -> Page<T> {
    let Some(size) = page_size.filter(|s| *s > 0) else {
        return empty_page(items);
    };
    let offset = query
        .cursor
        .as_ref()
        .and_then(|c| c.k.first())
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0);
    let end = (offset + size).min(items.len());
    let slice = items.get(offset..end).unwrap_or(&[]).to_vec();
    let next_cursor = if end < items.len() {
        let cur = CursorV1 {
            k: vec![end.to_string()],
            o: SortDir::Asc,
            // `decode` rejects empty sort tokens, so carry a non-empty `s`.
            s: "+id".to_owned(),
            f: None,
            d: "fwd".to_owned(),
        };
        // encode of this fixed struct cannot fail; `.ok()` keeps the fake free
        // of `expect`/`unwrap` (clippy::expect_used) — a None just ends paging.
        cur.encode().ok()
    } else {
        None
    };
    Page {
        items: slice,
        page_info: PageInfo {
            next_cursor,
            prev_cursor: None,
            limit: size as u64,
        },
    }
}

#[async_trait]
impl ResourceGroupReadHierarchy for InMemoryResourceGroupClient {
    async fn get_group(
        &self,
        _ctx: &SecurityContext,
        id: Uuid,
    ) -> Result<ResourceGroup, CanonicalError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        if let Some(err) = self.maybe_error() {
            return Err(err);
        }
        let script = match self.script.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        script.groups.get(&id).cloned().ok_or_else(|| {
            RgScope::not_found(format!("group {id} not in fake"))
                .with_resource(id.to_string())
                .create()
        })
    }

    async fn list_groups(
        &self,
        _ctx: &SecurityContext,
        _query: &ODataQuery,
    ) -> Result<Page<ResourceGroup>, CanonicalError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        if let Some(err) = self.maybe_error() {
            return Err(err);
        }
        let script = match self.script.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        Ok(empty_page(script.groups.values().cloned().collect()))
    }

    async fn get_group_descendants(
        &self,
        _ctx: &SecurityContext,
        group_id: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<ResourceGroupWithDepth>, CanonicalError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        if let Some(err) = self.maybe_error() {
            return Err(err);
        }
        let script = match self.script.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if let Some((StuckTarget::Descendants, mode)) = script.stuck_cursor {
            return Ok(stuck_page(mode, query));
        }
        let descendants = script
            .descendants
            .get(&group_id)
            .cloned()
            .unwrap_or_default();
        Ok(paginate(descendants, script.page_size, query))
    }

    async fn get_group_ancestors(
        &self,
        _ctx: &SecurityContext,
        _group_id: Uuid,
        _query: &ODataQuery,
    ) -> Result<Page<ResourceGroupWithDepth>, CanonicalError> {
        unreachable!(
            "InMemoryResourceGroupClient::get_group_ancestors unused in foundation/hierarchy"
        )
    }

    async fn list_memberships(
        &self,
        _ctx: &SecurityContext,
        query: &ODataQuery,
    ) -> Result<Page<ResourceGroupMembership>, CanonicalError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        if let Some(err) = self.maybe_error() {
            return Err(err);
        }
        let script = match self.script.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if let Some((StuckTarget::Memberships, mode)) = script.stuck_cursor {
            return Ok(stuck_page(mode, query));
        }
        let filter_ids = extract_group_id_in_filter(query);
        let items: Vec<ResourceGroupMembership> = match filter_ids {
            Some(allowed) => script
                .memberships
                .iter()
                .filter(|m| allowed.contains(&m.group_id))
                .cloned()
                .collect(),
            None => script.memberships.clone(),
        };
        Ok(paginate(items, script.page_size, query))
    }
}
