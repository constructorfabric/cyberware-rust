//! Name projection, request-local deduplication, and bounded failure behavior.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use account_management_sdk::{IdpUser, IdpUserFilterField, ListUsersQuery};
use async_trait::async_trait;
use parking_lot::Mutex;
use toolkit::ClientHub;
use toolkit_canonical_errors::CanonicalError;
use toolkit_odata::filter::{FilterNode, ODataValue};
use toolkit_odata::{Page, PageInfo};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::{
    ApprovalParticipants, LOOKUP_BATCH_SIZE, LOOKUP_BUDGET, LOOKUP_CONCURRENCY,
    ParticipantDirectory, ParticipantName, project_name,
};

/// Mutable upstream profiles; only the fake source owns these names.
#[derive(Default)]
struct Directory {
    calls: Mutex<Vec<Vec<Uuid>>>,
    users: Mutex<BTreeMap<Uuid, IdpUser>>,
}

#[async_trait]
impl ParticipantDirectory for Directory {
    async fn list_users(
        &self,
        _ctx: &SecurityContext,
        query: ListUsersQuery,
    ) -> Result<Page<IdpUser>, CanonicalError> {
        let ids = query_ids(&query);
        self.calls.lock().push(ids.clone());
        let users = self.users.lock();
        Ok(page(
            ids.iter().filter_map(|id| users.get(id).cloned()).collect(),
            None,
        ))
    }
}

/// Fail if enrichment ever asks for an unfiltered page or a malformed ID set.
fn query_ids(query: &ListUsersQuery) -> Vec<Uuid> {
    let Some(FilterNode::InList {
        field: IdpUserFilterField::Id,
        values,
    }) = &query.filter
    else {
        panic!("expected a typed ID-set lookup");
    };
    let ids: Vec<_> = values
        .iter()
        .map(|value| match value {
            ODataValue::Uuid(id) => *id,
            _ => panic!("expected a UUID"),
        })
        .collect();
    assert!(ids.len() <= LOOKUP_BATCH_SIZE);
    assert_eq!(query.pagination.top() as usize, ids.len());
    ids
}

/// One complete or continued provider page.
fn page(items: Vec<IdpUser>, cursor: Option<&str>) -> Page<IdpUser> {
    Page::new(
        items,
        PageInfo {
            next_cursor: cursor.map(str::to_owned),
            prev_cursor: None,
            limit: 200,
        },
    )
}

#[test]
fn names_use_only_current_provider_fields_in_precedence_order() {
    let id = Uuid::from_u128(1);
    let mut user = IdpUser::new(id, "  alice ")
        .with_display_name("  Alice Jones  ")
        .with_first_name(" Alice ")
        .with_last_name(" Smith ")
        .with_email("private@example.test");
    assert_eq!(
        project_name(&user),
        ParticipantName::Resolved("Alice Jones".into())
    );
    user.display_name = Some(" \t ".into());
    assert_eq!(
        project_name(&user),
        ParticipantName::Resolved("Alice Smith".into())
    );
    user.first_name = None;
    assert_eq!(
        project_name(&user),
        ParticipantName::Resolved("Smith".into())
    );
    user.last_name = None;
    assert_eq!(
        project_name(&user),
        ParticipantName::Resolved("alice".into())
    );
    user.username = " ".into();
    assert_eq!(project_name(&user), ParticipantName::Unavailable);
}

#[tokio::test]
async fn deduplicates_within_request_but_observes_renames_on_the_next_read() {
    let directory = Arc::new(Directory::default());
    let id = Uuid::from_u128(1);
    directory
        .users
        .lock()
        .insert(id, IdpUser::new(id, "before"));
    let service = ApprovalParticipants::with_directory(directory.clone());
    let ctx = SecurityContext::anonymous();
    let first = service.resolve(&ctx, [id, id, id]).await;
    assert_eq!(first[&id], ParticipantName::Resolved("before".into()));
    assert_eq!(*directory.calls.lock(), [vec![id]]);
    directory.users.lock().insert(id, IdpUser::new(id, "after"));
    let second = service.resolve(&ctx, [id]).await;
    assert_eq!(second[&id], ParticipantName::Resolved("after".into()));
    assert_eq!(*directory.calls.lock(), [vec![id], vec![id]]);
}

#[tokio::test]
async fn empty_page_never_reads_the_directory() {
    let directory = Arc::new(Directory::default());
    let service = ApprovalParticipants::with_directory(directory.clone());
    assert!(
        service
            .resolve(&SecurityContext::anonymous(), [])
            .await
            .is_empty()
    );
    assert!(directory.calls.lock().is_empty());
}

#[tokio::test]
async fn hundreds_of_participants_use_bounded_batches_instead_of_point_lookups() {
    let directory = Arc::new(Directory::default());
    for id in (1..=401).map(Uuid::from_u128) {
        directory
            .users
            .lock()
            .insert(id, IdpUser::new(id, "participant"));
    }
    let service = ApprovalParticipants::with_directory(directory.clone());
    let names = service
        .resolve(
            &SecurityContext::anonymous(),
            (1..=401).map(Uuid::from_u128),
        )
        .await;
    assert_eq!(names.len(), 401);
    assert!(
        names
            .values()
            .all(|name| matches!(name, ParticipantName::Resolved(_)))
    );
    assert_eq!(
        directory
            .calls
            .lock()
            .iter()
            .map(Vec::len)
            .collect::<Vec<_>>(),
        [200, 200, 1]
    );
}

/// Scripted pages exercise cursor handling through the same SDK query boundary.
struct PagedDirectory {
    pages: Mutex<VecDeque<Result<Page<IdpUser>, CanonicalError>>>,
    queries: Mutex<Vec<ListUsersQuery>>,
}

#[async_trait]
impl ParticipantDirectory for PagedDirectory {
    async fn list_users(
        &self,
        _ctx: &SecurityContext,
        query: ListUsersQuery,
    ) -> Result<Page<IdpUser>, CanonicalError> {
        query_ids(&query);
        self.queries.lock().push(query);
        self.pages.lock().pop_front().expect("no excess page calls")
    }
}

/// A directory whose every expected request has an explicit answer.
fn paged_directory(pages: Vec<Result<Page<IdpUser>, CanonicalError>>) -> Arc<PagedDirectory> {
    Arc::new(PagedDirectory {
        pages: Mutex::new(pages.into()),
        queries: Mutex::new(Vec::new()),
    })
}

#[tokio::test]
async fn batch_pagination_keeps_the_exact_filter_and_marks_absence_only_at_the_end() {
    let first = Uuid::from_u128(1);
    let second = Uuid::from_u128(2);
    let absent = Uuid::from_u128(3);
    let directory = paged_directory(vec![
        Ok(page(vec![IdpUser::new(first, "alice")], Some("page-2"))),
        Ok(page(vec![IdpUser::new(second, "bob")], None)),
    ]);
    let service = ApprovalParticipants::with_directory(directory.clone());
    let names = service
        .resolve(&SecurityContext::anonymous(), [first, second, absent])
        .await;
    assert_eq!(names[&first], ParticipantName::Resolved("alice".into()));
    assert_eq!(names[&second], ParticipantName::Resolved("bob".into()));
    assert_eq!(names[&absent], ParticipantName::NotFound);
    let queries = directory.queries.lock();
    assert_eq!(queries.len(), 2);
    assert_eq!(query_ids(&queries[0]), [first, second, absent]);
    assert_eq!(query_ids(&queries[1]), [first, second, absent]);
    assert!(queries[0].pagination.cursor().is_none());
    assert_eq!(queries[1].pagination.cursor(), Some("page-2"));
}

#[tokio::test]
async fn failed_later_page_preserves_known_names_but_does_not_claim_absence() {
    let first = Uuid::from_u128(1);
    let second = Uuid::from_u128(2);
    let directory = paged_directory(vec![
        Ok(page(vec![IdpUser::new(first, "alice")], Some("page-2"))),
        Err(CanonicalError::service_unavailable().create()),
    ]);
    let service = ApprovalParticipants::with_directory(directory);
    let names = service
        .resolve(&SecurityContext::anonymous(), [first, second])
        .await;
    assert_eq!(names[&first], ParticipantName::Resolved("alice".into()));
    assert_eq!(names[&second], ParticipantName::Unavailable);
}

#[tokio::test]
async fn empty_continued_page_and_duplicate_ids_are_not_authoritative_absence() {
    let id = Uuid::from_u128(1);
    for response in [
        page(Vec::new(), Some("never-progresses")),
        page(vec![IdpUser::new(id, "a"), IdpUser::new(id, "b")], None),
    ] {
        let directory = paged_directory(vec![Ok(response)]);
        let service = ApprovalParticipants::with_directory(directory.clone());
        assert_eq!(
            service.resolve(&SecurityContext::anonymous(), [id]).await[&id],
            ParticipantName::Unavailable
        );
        assert_eq!(directory.queries.lock().len(), 1);
    }
}

#[tokio::test]
async fn repeated_cursor_stops_without_unbounded_retries() {
    let first = Uuid::from_u128(1);
    let second = Uuid::from_u128(2);
    let third = Uuid::from_u128(3);
    let directory = paged_directory(vec![
        Ok(page(
            vec![IdpUser::new(first, "alice")],
            Some("same-cursor"),
        )),
        Ok(page(vec![IdpUser::new(second, "bob")], Some("same-cursor"))),
    ]);
    let service = ApprovalParticipants::with_directory(directory.clone());
    let names = service
        .resolve(&SecurityContext::anonymous(), [first, second, third])
        .await;
    assert_eq!(names[&third], ParticipantName::Unavailable);
    assert_eq!(directory.queries.lock().len(), 2);
}

#[tokio::test]
async fn mismatched_profile_id_is_not_disclosed() {
    let directory = Arc::new(Directory::default());
    let id = Uuid::from_u128(1);
    directory
        .users
        .lock()
        .insert(id, IdpUser::new(Uuid::from_u128(2), "someone else"));
    let service = ApprovalParticipants::with_directory(directory);
    assert_eq!(
        service.resolve(&SecurityContext::anonymous(), [id]).await[&id],
        ParticipantName::Unavailable
    );
}

#[tokio::test]
async fn optional_am_missing_is_explicitly_unavailable() {
    let service = ApprovalParticipants::new(Arc::new(ClientHub::new()));
    let id = Uuid::from_u128(1);
    assert_eq!(
        service.resolve(&SecurityContext::anonymous(), [id]).await[&id],
        ParticipantName::Unavailable
    );
}

/// An unanswering source records starts and cancellation without sleeping.
#[derive(Default)]
struct HungDirectory {
    started: AtomicUsize,
    cancelled: AtomicUsize,
    fast_id: Option<Uuid>,
}

/// Counts a dropped in-flight future.
struct Cancelled<'a>(&'a AtomicUsize);

impl Drop for Cancelled<'_> {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl ParticipantDirectory for HungDirectory {
    async fn list_users(
        &self,
        _ctx: &SecurityContext,
        query: ListUsersQuery,
    ) -> Result<Page<IdpUser>, CanonicalError> {
        let ids = query_ids(&query);
        self.started.fetch_add(1, Ordering::SeqCst);
        if self.fast_id.is_some_and(|id| ids.contains(&id)) {
            return Ok(page(
                ids.into_iter()
                    .map(|id| IdpUser::new(id, "available participant"))
                    .collect(),
                None,
            ));
        }
        let _cancelled = Cancelled(&self.cancelled);
        std::future::pending().await
    }
}

#[tokio::test(start_paused = true)]
async fn one_page_budget_bounds_parallelism_and_cancels_outstanding_reads() {
    let directory = Arc::new(HungDirectory::default());
    let service = ApprovalParticipants::with_directory(directory.clone());
    let start = tokio::time::Instant::now();
    let names = service
        .resolve(
            &SecurityContext::anonymous(),
            (1..=4000).map(Uuid::from_u128),
        )
        .await;
    assert_eq!(names.len(), 4000);
    assert!(
        names
            .values()
            .all(|name| *name == ParticipantName::Unavailable)
    );
    assert_eq!(directory.started.load(Ordering::SeqCst), LOOKUP_CONCURRENCY);
    assert_eq!(
        directory.cancelled.load(Ordering::SeqCst),
        LOOKUP_CONCURRENCY
    );
    // The timer wheel may round the shared deadline up by one millisecond.
    assert!(start.elapsed() <= LOOKUP_BUDGET + std::time::Duration::from_millis(1));
}

#[tokio::test(start_paused = true)]
async fn completed_names_survive_other_participants_timing_out() {
    let id = Uuid::from_u128(1);
    let directory = Arc::new(HungDirectory {
        fast_id: Some(id),
        ..Default::default()
    });
    let service = ApprovalParticipants::with_directory(directory.clone());
    let names = service
        .resolve(
            &SecurityContext::anonymous(),
            (1..=4000).map(Uuid::from_u128),
        )
        .await;
    assert_eq!(
        names[&id],
        ParticipantName::Resolved("available participant".into())
    );
    assert_eq!(
        names
            .values()
            .filter(|name| **name == ParticipantName::Unavailable)
            .count(),
        4000 - LOOKUP_BATCH_SIZE
    );
    assert_eq!(
        directory.started.load(Ordering::SeqCst),
        LOOKUP_CONCURRENCY + 1
    );
    assert_eq!(
        directory.cancelled.load(Ordering::SeqCst),
        LOOKUP_CONCURRENCY
    );
}
