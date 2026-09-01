//! Tests for [`super::ResourceGroupReadAdapter::group_names`] — the
//! batched group display-name read.
//!
//! What these pin down is the *shape of the upstream traffic*, not just
//! the returned names: a regression to one call per group id, or to a
//! query without an explicit limit (which the RG repository would resolve
//! to its own 25-row default and silently truncate a chunk), still
//! returns correct-looking names for small fixtures. Only the recorded
//! queries catch it.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::Mutex;
use resource_group_sdk::models::{GroupHierarchy, ResourceGroup, ResourceGroupWithDepth};
use toolkit_canonical_errors::CanonicalError;
use toolkit_odata::{ODataQuery, Page, PageInfo};
use toolkit_security::SecurityContext;
use uuid::{Uuid, uuid};

use super::{GROUP_NAME_CHUNK, ResourceGroupReadAdapter};
use crate::domain::rg_port::RbacRgRead;

const T1: Uuid = uuid!("11111111-1111-1111-1111-111111111111");
const G1: Uuid = uuid!("aaaaaaaa-0000-0000-0000-000000000001");
const G2: Uuid = uuid!("aaaaaaaa-0000-0000-0000-000000000002");

fn ctx() -> SecurityContext {
    SecurityContext::anonymous()
}

/// Recording `dyn ResourceGroupReadHierarchy`. `list_groups` answers
/// from a seeded `id -> (name, tenant)` table, honouring the `$filter`
/// only to the extent of returning the seeded groups the caller asked
/// for; every query it receives is recorded so tests can assert the
/// batching contract.
#[derive(Default)]
struct FakeReadHierarchy {
    groups: Mutex<Vec<ResourceGroup>>,
    calls: AtomicUsize,
    /// `(requested limit, number of id values in the `in` filter)` per
    /// `list_groups` call.
    seen: Mutex<Vec<(Option<u64>, usize)>>,
    /// When set, every `list_groups` call from this index onwards fails.
    /// Reproduces the shape that matters: an upstream that answers the
    /// first chunks of a multi-chunk read and then stops.
    fail_from_call: Option<usize>,
}

impl FakeReadHierarchy {
    fn with_group(self, id: Uuid, name: &str, tenant_id: Uuid) -> Self {
        self.groups.lock().push(ResourceGroup {
            id,
            code: "gts.cf.core.rg.type.v1~cf.core._.user_group.v1~".to_owned(),
            name: name.to_owned(),
            hierarchy: GroupHierarchy {
                parent_id: None,
                tenant_id,
            },
            metadata: None,
        });
        self
    }

    /// Fail every `list_groups` call from the (zero-based) `index`-th on.
    fn failing_from_call(mut self, index: usize) -> Self {
        self.fail_from_call = Some(index);
        self
    }

    fn list_calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// Ids carried by the query's top-level `id in (...)` filter.
    fn filter_ids(query: &ODataQuery) -> Vec<Uuid> {
        match query.filter() {
            Some(toolkit_odata::ast::Expr::In(_, values)) => values
                .iter()
                .filter_map(|v| match v {
                    toolkit_odata::ast::Expr::Value(toolkit_odata::ast::Value::Uuid(u)) => Some(*u),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    }
}

#[async_trait::async_trait]
impl resource_group_sdk::api::ResourceGroupReadHierarchy for FakeReadHierarchy {
    async fn get_group_descendants(
        &self,
        _ctx: &SecurityContext,
        _group_id: Uuid,
        _query: &ODataQuery,
    ) -> Result<Page<ResourceGroupWithDepth>, CanonicalError> {
        unimplemented!("FakeReadHierarchy: get_group_descendants is not exercised")
    }

    async fn get_group_ancestors(
        &self,
        _ctx: &SecurityContext,
        _group_id: Uuid,
        _query: &ODataQuery,
    ) -> Result<Page<ResourceGroupWithDepth>, CanonicalError> {
        unimplemented!("FakeReadHierarchy: get_group_ancestors is not exercised")
    }

    async fn list_groups(
        &self,
        _ctx: &SecurityContext,
        query: &ODataQuery,
    ) -> Result<Page<ResourceGroup>, CanonicalError> {
        let call_index = self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_from_call.is_some_and(|from| call_index >= from) {
            return Err(CanonicalError::internal("resource-group listing failed").create());
        }
        let wanted = Self::filter_ids(query);
        self.seen.lock().push((query.limit, wanted.len()));
        let items: Vec<ResourceGroup> = self
            .groups
            .lock()
            .iter()
            .filter(|g| wanted.contains(&g.id))
            .cloned()
            .collect();
        let limit = items.len() as u64;
        Ok(Page {
            items,
            page_info: PageInfo {
                next_cursor: None,
                prev_cursor: None,
                limit,
            },
        })
    }

    async fn get_group(
        &self,
        _ctx: &SecurityContext,
        _id: Uuid,
    ) -> Result<ResourceGroup, CanonicalError> {
        unimplemented!("FakeReadHierarchy: get_group is not exercised")
    }

    async fn list_memberships(
        &self,
        _ctx: &SecurityContext,
        _query: &ODataQuery,
    ) -> Result<Page<resource_group_sdk::models::ResourceGroupMembership>, CanonicalError> {
        unimplemented!("FakeReadHierarchy: list_memberships is not exercised")
    }
}

fn adapter(upstream: &Arc<FakeReadHierarchy>) -> ResourceGroupReadAdapter {
    ResourceGroupReadAdapter::new(
        Arc::clone(upstream) as Arc<dyn resource_group_sdk::api::ResourceGroupReadHierarchy>
    )
}

/// One `id in (...)` listing names every group on the page, and a
/// duplicated id does not add a call.
#[tokio::test]
async fn group_names_batches_ids_into_one_in_filter() {
    let upstream = Arc::new(
        FakeReadHierarchy::default()
            .with_group(G1, "Engineering", T1)
            .with_group(G2, "Finance", T1),
    );
    let adapter = adapter(&upstream);

    let out = adapter
        .group_names(&ctx(), &[G1, G2, G1])
        .await
        .expect("group names");

    assert_eq!(out.get(&G1).map(String::as_str), Some("Engineering"));
    assert_eq!(out.get(&G2).map(String::as_str), Some("Finance"));
    assert_eq!(upstream.list_calls(), 1, "duplicate ids must not add calls");
    // The chunk asked for exactly the ids it carried: without the
    // explicit limit the RG repository would default to 25 rows.
    assert_eq!(upstream.seen.lock().as_slice(), &[(Some(2), 2)]);
}

/// An id set larger than one chunk is split, and every chunk still
/// requests a limit equal to its own size.
#[tokio::test]
async fn group_names_chunks_oversized_id_sets() {
    let ids: Vec<Uuid> = (0..=GROUP_NAME_CHUNK).map(|_| Uuid::now_v7()).collect();
    let upstream = Arc::new(FakeReadHierarchy::default());
    let adapter = adapter(&upstream);

    let out = adapter
        .group_names(&ctx(), &ids)
        .await
        .expect("group names");

    assert!(out.is_empty(), "nothing seeded, so nothing resolves");
    assert_eq!(
        upstream.list_calls(),
        2,
        "one chunk per GROUP_NAME_CHUNK ids"
    );
    let seen = upstream.seen.lock().clone();
    assert_eq!(seen[0], (Some(GROUP_NAME_CHUNK as u64), GROUP_NAME_CHUNK));
    assert_eq!(seen[1], (Some(1), 1));
}

/// An empty id set must not touch the upstream at all — a page with no
/// group principals costs nothing.
#[tokio::test]
async fn group_names_of_nothing_makes_no_call() {
    let upstream = Arc::new(FakeReadHierarchy::default());
    let adapter = adapter(&upstream);

    let out = adapter.group_names(&ctx(), &[]).await.expect("group names");

    assert!(out.is_empty());
    assert_eq!(upstream.list_calls(), 0);
}

/// An id with no group behind it is absent from the map rather than an
/// error: a group deleted after the assignment was written must still
/// list, with its id and no name.
#[tokio::test]
async fn unknown_ids_are_absent_not_an_error() {
    let upstream = Arc::new(FakeReadHierarchy::default().with_group(G1, "Engineering", T1));
    let adapter = adapter(&upstream);

    let out = adapter
        .group_names(&ctx(), &[G1, G2])
        .await
        .expect("group names");

    assert_eq!(out.len(), 1);
    assert!(!out.contains_key(&G2));
}

/// A chunk that fails does not throw away the names the earlier chunks
/// already resolved.
///
/// This is the whole point of the partial-result contract: a page with
/// more group principals than one chunk holds would otherwise lose every
/// resolved name because the *last* listing timed out, turning a partial
/// outage into a total one for that page. The user-name reader makes the
/// same trade for the same reason.
#[tokio::test]
async fn a_failing_chunk_keeps_the_names_earlier_chunks_resolved() {
    let mut ids: Vec<Uuid> = (0..GROUP_NAME_CHUNK).map(|_| Uuid::now_v7()).collect();
    // One more id than a chunk holds, so the read takes two calls; the
    // second one fails.
    ids.push(G2);
    let mut upstream = FakeReadHierarchy::default().failing_from_call(1);
    for id in &ids[..GROUP_NAME_CHUNK] {
        upstream = upstream.with_group(*id, "Engineering", T1);
    }
    let upstream = Arc::new(upstream);
    let adapter = adapter(&upstream);

    let out = adapter
        .group_names(&ctx(), &ids)
        .await
        .expect("a partially resolved read MUST NOT fail");

    assert_eq!(
        out.len(),
        GROUP_NAME_CHUNK,
        "every name the first chunk resolved survives the second chunk's failure"
    );
    assert!(!out.contains_key(&G2));
    assert_eq!(
        upstream.list_calls(),
        2,
        "the drain stops at the first failure"
    );
}

/// When nothing at all could be resolved the error surfaces, so the caller
/// can log the cause once for the page instead of inferring an outage from
/// a silently empty map.
#[tokio::test]
async fn a_read_that_resolves_nothing_surfaces_the_error() {
    let upstream = Arc::new(
        FakeReadHierarchy::default()
            .with_group(G1, "Engineering", T1)
            .failing_from_call(0),
    );
    let adapter = adapter(&upstream);

    let err = adapter
        .group_names(&ctx(), &[G1])
        .await
        .expect_err("a read that produced no names at all MUST report why");

    assert!(matches!(
        err,
        crate::domain::rg_port::RbacRgReadError::Upstream(_)
    ));
}

/// A blank upstream name is dropped rather than passed through: an absent
/// field renders as the group id, while `"   "` renders as an empty cell
/// that reads as a bug.
#[tokio::test]
async fn blank_group_names_are_dropped() {
    let upstream = Arc::new(
        FakeReadHierarchy::default()
            .with_group(G1, "   ", T1)
            .with_group(G2, "Finance", T1),
    );
    let adapter = adapter(&upstream);

    let out = adapter
        .group_names(&ctx(), &[G1, G2])
        .await
        .expect("group names");

    assert!(
        !out.contains_key(&G1),
        "a whitespace-only name MUST be treated as no name"
    );
    assert_eq!(out.get(&G2).map(String::as_str), Some("Finance"));
}

/// A name with incidental whitespace is trimmed rather than dropped —
/// the same normalization every other name source applies.
#[tokio::test]
async fn group_names_are_trimmed() {
    let upstream = Arc::new(FakeReadHierarchy::default().with_group(G1, "  Engineering\n", T1));
    let adapter = adapter(&upstream);

    let out = adapter
        .group_names(&ctx(), &[G1])
        .await
        .expect("group names");

    assert_eq!(out.get(&G1).map(String::as_str), Some("Engineering"));
}
