//! Unit tests for [`TypesRegistryPermissionCatalog`] backed by
//! `types_registry_sdk::testing::MockTypesRegistryClient`.

#![allow(clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use serde_json::json;
use types_registry_sdk::testing::{MockTypesRegistryClient, make_test_instance};
use types_registry_sdk::{GtsInstance, TypesRegistryClient};

use super::TypesRegistryPermissionCatalog;
use crate::domain::permission_catalog::{
    CatalogCursor, CatalogCursorDirection, PermissionCatalog, PermissionCatalogError,
    PermissionCatalogFilter,
};

/// Limit big enough to never truncate a test catalog.
const UNLIMITED: u32 = u32::MAX;

const TYPE_PREFIX: &str = "gts.cf.toolkit.authz.permission.v1~";

/// Build a permission `GtsInstance`. GTS rules require ≥5 dot-tokens
/// after `~`, so the helper prepends a 4-token namespace chain.
fn perm_instance(name_token: &str, action: &str, resource_type: &str) -> GtsInstance {
    let id = format!("{TYPE_PREFIX}cf.core.rbac.{name_token}.v1");
    make_test_instance(
        &id,
        json!({
            "id": id,
            "resource_type": resource_type,
            "action": action,
            "display_name": format!("Display for {name_token}"),
        }),
    )
}

fn build_catalog(client: MockTypesRegistryClient) -> TypesRegistryPermissionCatalog {
    let trait_obj: Arc<dyn TypesRegistryClient> = Arc::new(client);
    TypesRegistryPermissionCatalog::new(trait_obj)
}

// -----------------------------------------------------------------------------
// fetch_all-driven branches.
// -----------------------------------------------------------------------------

#[tokio::test]
async fn list_permissions_happy_path_sorted_by_id() {
    let mock = MockTypesRegistryClient::new().with_instances([
        perm_instance("zeta", "read", "gts.cf.test.example.thing.v1~"),
        perm_instance("alpha", "write", "gts.cf.test.example.thing.v1~"),
    ]);
    let catalog = build_catalog(mock);

    let page = catalog
        .list_permissions(PermissionCatalogFilter::default(), None, UNLIMITED)
        .await
        .expect("happy path must succeed");

    assert_eq!(page.items.len(), 2);
    // Sorted ascending by id.
    assert!(
        page.items[0].id.ends_with("alpha.v1"),
        "got: {}",
        page.items[0].id
    );
    assert!(
        page.items[1].id.ends_with("zeta.v1"),
        "got: {}",
        page.items[1].id
    );
    assert_eq!(page.items[0].action, "write");
    assert_eq!(page.items[0].resource_type, "gts.cf.test.example.thing.v1~");
    assert_eq!(page.items[0].display_name, "Display for alpha");
    assert!(
        page.next_cursor.is_none(),
        "unlimited page MUST NOT emit a next_cursor"
    );
}

#[tokio::test]
async fn list_permissions_propagates_registry_error_as_registry_variant() {
    let mock = MockTypesRegistryClient::new().with_list_error(
        toolkit_canonical_errors::CanonicalError::service_unavailable()
            .with_detail("registry is down")
            .create(),
    );
    let catalog = build_catalog(mock);

    let err = catalog
        .list_permissions(PermissionCatalogFilter::default(), None, UNLIMITED)
        .await
        .expect_err("registry error must surface");

    match err {
        PermissionCatalogError::Registry(msg) => {
            assert!(msg.contains("registry is down"), "msg={msg}");
        }
        other => panic!("expected Registry, got {other:?}"),
    }
}

#[tokio::test]
async fn list_permissions_propagates_malformed_payload_as_deserialize_variant() {
    // Missing required `display_name` — serde fails.
    let bad_id = format!("{TYPE_PREFIX}cf.core.rbac.broken.v1");
    let bad = make_test_instance(
        &bad_id,
        json!({
            "id": bad_id,
            "resource_type": "gts.cf.test.example.thing.v1~",
            "action": "read",
        }),
    );
    let mock = MockTypesRegistryClient::new().with_instances([bad]);
    let catalog = build_catalog(mock);

    let err = catalog
        .list_permissions(PermissionCatalogFilter::default(), None, UNLIMITED)
        .await
        .expect_err("deserialize error must surface");

    match err {
        PermissionCatalogError::Deserialize { id, cause } => {
            assert!(id.ends_with("broken.v1"), "id={id}");
            assert!(cause.contains("display_name"), "cause={cause}");
        }
        other => panic!("expected Deserialize, got {other:?}"),
    }
}

// -----------------------------------------------------------------------------
// list_permissions filters
// -----------------------------------------------------------------------------

#[tokio::test]
async fn list_permissions_filter_by_action_only() {
    let mock = MockTypesRegistryClient::new().with_instances([
        perm_instance("read_thing", "read", "gts.cf.test.example.thing.v1~"),
        perm_instance("write_thing", "write", "gts.cf.test.example.thing.v1~"),
        perm_instance("read_other", "read", "gts.cf.test.example.other.v1~"),
    ]);
    let catalog = build_catalog(mock);

    let page = catalog
        .list_permissions(
            PermissionCatalogFilter {
                action: Some("read".into()),
                resource_type_prefix: None,
            },
            None,
            UNLIMITED,
        )
        .await
        .expect("filter must succeed");

    assert_eq!(page.items.len(), 2);
    assert!(page.items.iter().all(|p| p.action == "read"));
}

#[tokio::test]
async fn list_permissions_filter_by_resource_type_prefix_only() {
    let mock = MockTypesRegistryClient::new().with_instances([
        perm_instance("read_thing", "read", "gts.cf.test.example.thing.v1~"),
        perm_instance("write_thing", "write", "gts.cf.test.example.thing.v1~"),
        perm_instance("read_other", "read", "gts.cf.test.example.other.v1~"),
    ]);
    let catalog = build_catalog(mock);

    // Derive prefix at runtime — a bare GTS-shaped literal trips DE0901.
    let prefix = "gts.cf.test.example.thing.v1~".trim_end_matches('~');
    let page = catalog
        .list_permissions(
            PermissionCatalogFilter {
                action: None,
                resource_type_prefix: Some(prefix.to_owned()),
            },
            None,
            UNLIMITED,
        )
        .await
        .expect("filter must succeed");

    assert_eq!(page.items.len(), 2);
    assert!(
        page.items
            .iter()
            .all(|p| p.resource_type.starts_with(prefix))
    );
}

#[tokio::test]
async fn list_permissions_filter_by_action_and_prefix_intersects() {
    let mock = MockTypesRegistryClient::new().with_instances([
        perm_instance("read_thing", "read", "gts.cf.test.example.thing.v1~"),
        perm_instance("write_thing", "write", "gts.cf.test.example.thing.v1~"),
        perm_instance("read_other", "read", "gts.cf.test.example.other.v1~"),
    ]);
    let catalog = build_catalog(mock);

    let prefix = "gts.cf.test.example.thing.v1~".trim_end_matches('~');
    let page = catalog
        .list_permissions(
            PermissionCatalogFilter {
                action: Some("read".into()),
                resource_type_prefix: Some(prefix.to_owned()),
            },
            None,
            UNLIMITED,
        )
        .await
        .expect("filter must succeed");

    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].action, "read");
    assert!(page.items[0].resource_type.starts_with(prefix));
}

// -----------------------------------------------------------------------------
// Pagination — cursor + limit pushed into the catalog.
// -----------------------------------------------------------------------------

/// Five-entry id-sorted catalog used by the pagination tests below.
/// `perm_instance("name", action, resource)` synthesizes the id from
/// `name`, so a–e produce ids ending in `a.v1` < `b.v1` < … < `e.v1`.
fn five_entry_catalog() -> MockTypesRegistryClient {
    MockTypesRegistryClient::new().with_instances([
        perm_instance("a", "read", "gts.cf.test.example.thing.v1~"),
        perm_instance("b", "read", "gts.cf.test.example.thing.v1~"),
        perm_instance("c", "write", "gts.cf.test.example.thing.v1~"),
        perm_instance("d", "read", "gts.cf.test.example.thing.v1~"),
        perm_instance("e", "delete", "gts.cf.test.example.thing.v1~"),
    ])
}

#[tokio::test]
async fn limit_enforced_returns_partial_page_with_next_cursor() {
    let catalog = build_catalog(five_entry_catalog());

    let page = catalog
        .list_permissions(PermissionCatalogFilter::default(), None, 2)
        .await
        .expect("limit=2 over 5 entries must succeed");

    assert_eq!(page.items.len(), 2);
    assert!(page.items[0].id.ends_with("a.v1"));
    assert!(page.items[1].id.ends_with("b.v1"));
    let next = page
        .next_cursor
        .expect("more items remain \u{2014} next_cursor MUST be Some");
    assert!(
        next.id.ends_with("b.v1"),
        "next_cursor.id MUST be the last emitted item's id, got {}",
        next.id
    );
    // First page has nothing before it.
    assert!(
        page.prev_cursor.is_none(),
        "first page MUST NOT carry a prev_cursor"
    );
}

#[tokio::test]
async fn cursor_advances_past_emitted_items() {
    let catalog = build_catalog(five_entry_catalog());

    let page_1 = catalog
        .list_permissions(PermissionCatalogFilter::default(), None, 2)
        .await
        .expect("page 1 ok");
    let cursor = page_1
        .next_cursor
        .clone()
        .expect("page 1 MUST have a next_cursor");

    let page_2 = catalog
        .list_permissions(PermissionCatalogFilter::default(), Some(cursor), 2)
        .await
        .expect("page 2 ok");

    assert_eq!(page_2.items.len(), 2);
    assert!(page_2.items[0].id.ends_with("c.v1"));
    assert!(page_2.items[1].id.ends_with("d.v1"));
    // No overlap with page 1.
    for emitted in &page_1.items {
        assert!(
            !page_2.items.iter().any(|p| p.id == emitted.id),
            "page 2 MUST NOT re-emit page 1 entry {}",
            emitted.id
        );
    }
}

#[tokio::test]
async fn prev_cursor_walks_back_to_the_preceding_page() {
    let catalog = build_catalog(five_entry_catalog());

    // Forward to the middle page (c, d).
    let page_1 = catalog
        .list_permissions(PermissionCatalogFilter::default(), None, 2)
        .await
        .expect("page 1 ok");
    let page_2 = catalog
        .list_permissions(
            PermissionCatalogFilter::default(),
            Some(page_1.next_cursor.expect("page 1 has next")),
            2,
        )
        .await
        .expect("page 2 ok");
    assert!(page_2.items[0].id.ends_with("c.v1"));
    assert!(page_2.items[1].id.ends_with("d.v1"));

    // Page 2 sits in the middle: it has both a prev and a next cursor.
    let prev = page_2
        .prev_cursor
        .expect("middle page MUST carry a prev_cursor");
    assert_eq!(prev.direction, CatalogCursorDirection::Backward);
    assert!(
        page_2.next_cursor.is_some(),
        "middle page MUST carry a next_cursor (e remains)"
    );

    // Walking back with that prev cursor reproduces page 1 (a, b).
    let back = catalog
        .list_permissions(PermissionCatalogFilter::default(), Some(prev), 2)
        .await
        .expect("backward page ok");
    assert_eq!(back.items.len(), 2);
    assert!(back.items[0].id.ends_with("a.v1"));
    assert!(back.items[1].id.ends_with("b.v1"));
    // We're back at the start: no prev, but a next pointing forward again.
    assert!(
        back.prev_cursor.is_none(),
        "first page reached via backward walk MUST NOT carry a prev_cursor"
    );
    assert!(
        back.next_cursor.is_some(),
        "backward-reached first page still has entries after it"
    );
}

#[tokio::test]
async fn cursor_at_or_past_last_id_returns_empty_page() {
    let catalog = build_catalog(five_entry_catalog());

    // Cursor pointing at the last entry's id.
    let last_id = format!("{TYPE_PREFIX}cf.core.rbac.e.v1");
    let page = catalog
        .list_permissions(
            PermissionCatalogFilter::default(),
            Some(CatalogCursor::forward(last_id)),
            10,
        )
        .await
        .expect("cursor past last id ok");

    assert!(
        page.items.is_empty(),
        "cursor at the last id MUST yield no items, got {:?}",
        page.items
    );
    assert!(
        page.next_cursor.is_none(),
        "no items left \u{2014} next_cursor MUST be None"
    );
}

#[tokio::test]
async fn cursor_with_unknown_last_id_is_rejected_as_invalid() {
    let catalog = build_catalog(five_entry_catalog());

    // A `last_id` absent from the catalog — the shape a garbage (but
    // base64-decodable) or foreign/schema-mismatched cursor takes after
    // decode. MUST be rejected as `InvalidCursor` (REST → 400), NOT
    // silently seeked to the insertion point and returned as a page.
    let result = catalog
        .list_permissions(
            PermissionCatalogFilter::default(),
            Some(CatalogCursor::forward(
                "no-such-id-in-the-catalog".to_owned(),
            )),
            10,
        )
        .await;

    assert!(
        matches!(result, Err(PermissionCatalogError::InvalidCursor)),
        "unknown cursor id MUST be rejected as InvalidCursor, got {result:?}"
    );
}

#[tokio::test]
async fn filter_and_cursor_compose() {
    let catalog = build_catalog(five_entry_catalog());

    // Filter to action=read (matches a, b, d in the seed); take a
    // limit-1 page, then page through using the returned cursor.
    let filter = || PermissionCatalogFilter {
        action: Some("read".into()),
        resource_type_prefix: None,
    };

    let page_1 = catalog
        .list_permissions(filter(), None, 1)
        .await
        .expect("filtered page 1 ok");
    assert_eq!(page_1.items.len(), 1);
    assert!(page_1.items[0].id.ends_with("a.v1"));
    let cursor_1 = page_1.next_cursor.expect("more reads remain after a.v1");

    let page_2 = catalog
        .list_permissions(filter(), Some(cursor_1), 1)
        .await
        .expect("filtered page 2 ok");
    assert_eq!(page_2.items.len(), 1);
    assert!(
        page_2.items[0].id.ends_with("b.v1"),
        "page 2 MUST skip over the non-read 'c' entry and land on 'd' next; \
         this page returns 'b' as the next read after 'a'"
    );
    let cursor_2 = page_2
        .next_cursor
        .expect("'d' (the third read) still remains after 'b'");

    let page_3 = catalog
        .list_permissions(filter(), Some(cursor_2), 1)
        .await
        .expect("filtered page 3 ok");
    assert_eq!(page_3.items.len(), 1);
    assert!(
        page_3.items[0].id.ends_with("d.v1"),
        "page 3 MUST skip past 'c' (action=write) and land on 'd' (action=read)"
    );
    assert!(
        page_3.next_cursor.is_none(),
        "'d' is the last read entry \u{2014} next_cursor MUST be None"
    );
}

// -----------------------------------------------------------------------------
// exists()
// -----------------------------------------------------------------------------

#[tokio::test]
async fn exists_returns_true_for_present_pair() {
    let mock = MockTypesRegistryClient::new().with_instances([perm_instance(
        "read_thing",
        "read",
        "gts.cf.test.example.thing.v1~",
    )]);
    let catalog = build_catalog(mock);

    let present = catalog
        .exists("read", "gts.cf.test.example.thing.v1~")
        .await
        .expect("exists must succeed");
    assert!(present);
}

#[tokio::test]
async fn exists_returns_false_for_absent_pair() {
    let mock = MockTypesRegistryClient::new().with_instances([perm_instance(
        "read_thing",
        "read",
        "gts.cf.test.example.thing.v1~",
    )]);
    let catalog = build_catalog(mock);

    let present = catalog
        .exists("delete", "gts.cf.test.example.thing.v1~")
        .await
        .expect("exists must succeed");
    assert!(!present, "delete is NOT registered for things.v1~");
}

#[tokio::test]
async fn exists_propagates_registry_failure() {
    let mock = MockTypesRegistryClient::new()
        .with_list_error(types_registry_sdk::testing::internal("noooope"));
    let catalog = build_catalog(mock);

    let err = catalog
        .exists("read", "gts.cf.test.example.thing.v1~")
        .await
        .expect_err("registry error must surface from exists");

    assert!(matches!(err, PermissionCatalogError::Registry(_)));
}

// -----------------------------------------------------------------------------
// Snapshot cache.
// -----------------------------------------------------------------------------

/// Helper: build a catalog and return both it and a handle to the
/// underlying mock so the test can read `list_instance_calls()`.
fn build_catalog_with_mock(
    mock: MockTypesRegistryClient,
    cache_ttl: std::time::Duration,
) -> (TypesRegistryPermissionCatalog, Arc<MockTypesRegistryClient>) {
    let mock_arc = Arc::new(mock);
    let trait_obj: Arc<dyn TypesRegistryClient> =
        Arc::clone(&mock_arc) as Arc<dyn TypesRegistryClient>;
    (
        TypesRegistryPermissionCatalog::with_cache_ttl(trait_obj, cache_ttl),
        mock_arc,
    )
}

#[tokio::test]
async fn cache_serves_repeat_calls_from_one_upstream_fetch() {
    let mock = MockTypesRegistryClient::new().with_instances([perm_instance(
        "read_thing",
        "read",
        "gts.cf.test.example.thing.v1~",
    )]);
    let (catalog, mock_handle) = build_catalog_with_mock(mock, super::DEFAULT_CACHE_TTL);

    // Drive three back-to-back `exists` probes; they all share one
    // cached snapshot, so the upstream is hit exactly once.
    let _ = catalog
        .exists("read", "gts.cf.test.example.thing.v1~")
        .await
        .expect("first call ok");
    let _ = catalog
        .exists("write", "gts.cf.test.example.thing.v1~")
        .await
        .expect("second call ok");
    let _ = catalog
        .exists("read", "gts.cf.test.example.thing.v1~")
        .await
        .expect("third call ok");

    assert_eq!(
        mock_handle.list_instance_calls(),
        1,
        "cache MUST collapse three back-to-back calls into one upstream `list_instances`"
    );
}

#[tokio::test]
async fn cache_disabled_when_ttl_is_zero() {
    let mock = MockTypesRegistryClient::new().with_instances([perm_instance(
        "read_thing",
        "read",
        "gts.cf.test.example.thing.v1~",
    )]);
    let (catalog, mock_handle) = build_catalog_with_mock(mock, std::time::Duration::ZERO);

    let _ = catalog
        .exists("read", "gts.cf.test.example.thing.v1~")
        .await
        .expect("first call ok");
    let _ = catalog
        .exists("read", "gts.cf.test.example.thing.v1~")
        .await
        .expect("second call ok");

    assert_eq!(
        mock_handle.list_instance_calls(),
        2,
        "TTL=0 MUST disable the cache; each call refetches"
    );
}

#[tokio::test]
async fn cache_refreshes_after_ttl_expiry() {
    let mock = MockTypesRegistryClient::new().with_instances([perm_instance(
        "read_thing",
        "read",
        "gts.cf.test.example.thing.v1~",
    )]);
    // Very short TTL so the second call falls outside the freshness
    // window after a tiny `sleep`.
    let (catalog, mock_handle) = build_catalog_with_mock(mock, std::time::Duration::from_millis(5));

    let _ = catalog
        .exists("read", "gts.cf.test.example.thing.v1~")
        .await
        .expect("first call ok");
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    let _ = catalog
        .exists("read", "gts.cf.test.example.thing.v1~")
        .await
        .expect("second call ok");

    assert_eq!(
        mock_handle.list_instance_calls(),
        2,
        "second call after TTL expiry MUST refetch from the registry"
    );
}

#[tokio::test]
async fn cache_does_not_poison_on_registry_error() {
    // Cache-write path is gated on `fetch_from_registry` returning
    // `Ok(...)` — an error path never reaches the swap. Drive it by
    // building a catalog whose mock fails on every call, then assert
    // a subsequent call (still within the cache window) re-tries the
    // registry rather than caching the error. We see two
    // `list_instance_calls` instead of one.
    let mock = MockTypesRegistryClient::new()
        .with_list_error(types_registry_sdk::testing::internal("registry-down"));
    let (catalog, mock_handle) = build_catalog_with_mock(mock, super::DEFAULT_CACHE_TTL);

    let _ = catalog
        .exists("read", "gts.cf.test.example.thing.v1~")
        .await
        .expect_err("first call MUST surface registry error");
    let _ = catalog
        .exists("read", "gts.cf.test.example.thing.v1~")
        .await
        .expect_err("second call MUST also hit the registry \u{2014} errors are not cached");

    assert_eq!(
        mock_handle.list_instance_calls(),
        2,
        "an upstream error MUST NOT be cached; both calls refetch"
    );
}

// -----------------------------------------------------------------------------
// Staleness-budget boundary.
// -----------------------------------------------------------------------------

/// Tiny `TypesRegistryClient` whose `list_instances` mode can flip
/// mid-test. Other trait methods are unreachable: the catalog only
/// calls `list_instances`, so stubs panic loudly if a future refactor
/// adds an unexpected upstream call.
mod switchable {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use async_trait::async_trait;
    use toolkit_canonical_errors::CanonicalError;
    use types_registry_sdk::{
        GtsInstance, GtsTypeSchema, InstanceQuery, RegisterResult, TypeSchemaQuery,
        TypesRegistryClient,
    };
    use uuid::Uuid;

    pub(super) struct SwitchableClient {
        instances: Vec<GtsInstance>,
        failing: Arc<AtomicBool>,
    }

    impl SwitchableClient {
        pub(super) fn new(instances: Vec<GtsInstance>) -> (Arc<Self>, Arc<AtomicBool>) {
            let failing = Arc::new(AtomicBool::new(false));
            (
                Arc::new(Self {
                    instances,
                    failing: Arc::clone(&failing),
                }),
                failing,
            )
        }
    }

    #[async_trait]
    impl TypesRegistryClient for SwitchableClient {
        async fn list_instances(
            &self,
            _query: InstanceQuery,
        ) -> Result<Vec<GtsInstance>, CanonicalError> {
            if self.failing.load(Ordering::SeqCst) {
                Err(types_registry_sdk::testing::internal(
                    "simulated registry outage",
                ))
            } else {
                Ok(self.instances.clone())
            }
        }

        async fn register(
            &self,
            _: Vec<serde_json::Value>,
        ) -> Result<Vec<RegisterResult>, CanonicalError> {
            unreachable!("SwitchableClient: catalog only calls list_instances");
        }
        async fn register_type_schemas(
            &self,
            _: Vec<serde_json::Value>,
        ) -> Result<Vec<RegisterResult>, CanonicalError> {
            unreachable!("SwitchableClient: catalog only calls list_instances");
        }
        async fn get_type_schema(&self, _: &str) -> Result<GtsTypeSchema, CanonicalError> {
            unreachable!("SwitchableClient: catalog only calls list_instances");
        }
        async fn get_type_schema_by_uuid(&self, _: Uuid) -> Result<GtsTypeSchema, CanonicalError> {
            unreachable!("SwitchableClient: catalog only calls list_instances");
        }
        async fn get_type_schemas(
            &self,
            _: Vec<String>,
        ) -> HashMap<String, Result<GtsTypeSchema, CanonicalError>> {
            unreachable!("SwitchableClient: catalog only calls list_instances");
        }
        async fn get_type_schemas_by_uuid(
            &self,
            _: Vec<Uuid>,
        ) -> HashMap<Uuid, Result<GtsTypeSchema, CanonicalError>> {
            unreachable!("SwitchableClient: catalog only calls list_instances");
        }
        async fn list_type_schemas(
            &self,
            _: TypeSchemaQuery,
        ) -> Result<Vec<GtsTypeSchema>, CanonicalError> {
            unreachable!("SwitchableClient: catalog only calls list_instances");
        }
        async fn register_instances(
            &self,
            _: Vec<serde_json::Value>,
        ) -> Result<Vec<RegisterResult>, CanonicalError> {
            unreachable!("SwitchableClient: catalog only calls list_instances");
        }
        async fn get_instance(&self, _: &str) -> Result<GtsInstance, CanonicalError> {
            unreachable!("SwitchableClient: catalog only calls list_instances");
        }
        async fn get_instance_by_uuid(&self, _: Uuid) -> Result<GtsInstance, CanonicalError> {
            unreachable!("SwitchableClient: catalog only calls list_instances");
        }
        async fn get_instances(
            &self,
            _: Vec<String>,
        ) -> HashMap<String, Result<GtsInstance, CanonicalError>> {
            unreachable!("SwitchableClient: catalog only calls list_instances");
        }
        async fn get_instances_by_uuid(
            &self,
            _: Vec<Uuid>,
        ) -> HashMap<Uuid, Result<GtsInstance, CanonicalError>> {
            unreachable!("SwitchableClient: catalog only calls list_instances");
        }
    }
}

/// Across a registry outage, the catalog must
///   1. keep serving the warm cache up to `stale_threshold` past TTL
///      (graceful degradation across brief upstream blips), then
///   2. surface `PermissionCatalogError::Registry` once the snapshot
///      crosses the staleness budget (so sustained outages become
///      visible as 503 at the REST surface instead of dragging stale
///      authorisation metadata indefinitely).
///
/// Uses real-time sleeps with deliberately tiny TTL / threshold so
/// the test completes in well under a second. The catalog's
/// `Instant` comparisons would need a clock injection layer to drive
/// virtual time — out of scope for this fix.
#[tokio::test]
async fn stale_503_serves_within_threshold_then_fails_loud_past_it() {
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    let instance = perm_instance("read_thing", "read", "gts.cf.test.example.thing.v1~");
    let (client, failing) = switchable::SwitchableClient::new(vec![instance]);

    // 10ms TTL + 60ms stale_threshold. The test sequence:
    //   t = 0    : warm the cache (registry healthy).
    //   t = 0+   : flip the upstream into failure mode.
    //   t = 20ms : past TTL, well within stale_threshold → serve stale.
    //   t = 80ms : past stale_threshold → loud fail.
    let cache_ttl = Duration::from_millis(10);
    let stale_threshold = Duration::from_millis(60);
    let catalog = TypesRegistryPermissionCatalog::with_cache_ttl_and_stale_threshold(
        Arc::clone(&client) as Arc<dyn TypesRegistryClient>,
        cache_ttl,
        stale_threshold,
    );

    // Phase 1: warm the cache.
    catalog
        .exists("read", "gts.cf.test.example.thing.v1~")
        .await
        .expect("warm-up call against healthy registry MUST succeed");

    // Trip the upstream into outage mode.
    failing.store(true, Ordering::SeqCst);

    // Phase 2: past TTL, within stale_threshold. Refresh fails but
    // the snapshot is still inside the staleness budget, so the
    // catalog returns the cached row.
    tokio::time::sleep(Duration::from_millis(20)).await;
    let still_ok = catalog
        .exists("read", "gts.cf.test.example.thing.v1~")
        .await
        .expect("within stale_threshold and registry down, catalog MUST serve the stale snapshot");
    assert!(
        still_ok,
        "stale snapshot still contains the warm-up permission"
    );

    // Phase 3: past stale_threshold. The catalog must refuse and
    // surface the registry error so REST returns 503.
    tokio::time::sleep(Duration::from_millis(60)).await;
    let err = catalog
        .exists("read", "gts.cf.test.example.thing.v1~")
        .await
        .expect_err(
            "past stale_threshold + registry down MUST surface PermissionCatalogError::Registry",
        );
    assert!(
        matches!(err, PermissionCatalogError::Registry(_)),
        "expected PermissionCatalogError::Registry (mapped to 503 at REST), got {err:?}"
    );
}
