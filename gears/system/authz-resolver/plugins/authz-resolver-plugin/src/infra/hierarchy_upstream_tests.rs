#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
//! Unit tests for the hierarchy upstream adapter: page draining, cursor
//! handling, `IN`-list narrowing, non-UUID filtering, and SDK error mapping.
//!
//! They sit with the adapter so a drain can be exercised directly, without
//! reaching through `HierarchyClient` and its cache.

use std::sync::Arc;

use tenant_resolver_sdk::api::TenantResolverClient;
use uuid::Uuid;

use super::*;
use crate::domain::hierarchy_upstream::{GroupSubtreeFetch, HierarchyUpstream};
use crate::test_support::{
    InMemoryResourceGroupClient, InMemoryTenantResolverClient, StuckCursor, StuckTarget,
};

// #17: `parse_resource_ids` is a fail-safe filter on authorization data — a
// membership row whose `resource_id` is not a UUID cannot be authorized, so it
// is dropped (and the survivors are sorted + deduped). Pin that behavior so a
// regression (dropping valid ids, or keeping malformed ones) is caught.
#[test]
fn parse_resource_ids_drops_non_uuid_rows_and_dedups() {
    use resource_group_sdk::models::ResourceGroupMembership;

    let group_id = Uuid::from_u128(1);
    let valid_a = Uuid::from_u128(0xAAAA);
    let valid_b = Uuid::from_u128(0xBBBB);
    let mk = |rid: &str| ResourceGroupMembership {
        group_id,
        resource_type: "gts.cf.core.resources.test.v1~".to_owned(),
        resource_id: rid.to_owned(),
    };
    let memberships = vec![
        mk(&valid_a.to_string()),
        mk("not-a-uuid"), // dropped: unparseable
        mk(&valid_b.to_string()),
        mk(&valid_a.to_string()), // duplicate → deduped
        mk(""),                   // dropped: empty
    ];

    let ids = parse_resource_ids(&memberships);

    let mut expected = vec![valid_a, valid_b];
    expected.sort();
    assert_eq!(
        ids, expected,
        "only valid UUIDs survive, sorted and deduped; non-UUID rows dropped"
    );
}

// #1: the resolver paginates (cursor-based). The client must FOLLOW the cursor
// and accumulate every page — reading only the first page silently truncated
// the materialized allow-set. With page_size=2 over 5 descendant groups + 5
// memberships, a first-page-only reader would surface ~2; the drain must yield
// all 5 (and all 5 require BOTH the descendants drain — to put every group in
// the subtree — and the memberships drain).
#[tokio::test]
async fn group_subtree_drains_all_pages_not_just_the_first() {
    use resource_group_sdk::models::{
        GroupHierarchyWithDepth, ResourceGroupMembership, ResourceGroupWithDepth,
    };

    let tenant = Uuid::from_u128(0xAB);
    let root_group = Uuid::from_u128(0x100);
    let desc_ids: Vec<Uuid> = (1..=5).map(|i| Uuid::from_u128(0x200 + i)).collect();

    let descendants: Vec<ResourceGroupWithDepth> = desc_ids
        .iter()
        .map(|gid| ResourceGroupWithDepth {
            id: *gid,
            code: "gts.cf.core.rg.type.v1~test.v1~".to_owned(),
            name: format!("group-{gid:x}"),
            hierarchy: GroupHierarchyWithDepth {
                parent_id: None,
                tenant_id: tenant,
                depth: 0,
            },
            metadata: None,
        })
        .collect();
    let mut memberships: Vec<ResourceGroupMembership> = desc_ids
        .iter()
        .enumerate()
        .map(|(i, gid)| ResourceGroupMembership {
            group_id: *gid,
            resource_type: "gts.cf.core.resources.test.v1~".to_owned(),
            resource_id: Uuid::from_u128(0x300 + i as u128).to_string(),
        })
        .collect();
    // Decoys: memberships in groups OUTSIDE the granted subtree. Without them
    // the fixture cannot distinguish a narrowed drain from an un-narrowed one —
    // the fake returns everything when it sees no `group_id in (...)` filter,
    // and "everything" IS the in-subtree set when nothing else is seeded. With
    // them, dropping the narrowing surfaces these ids and the assertion fails.
    let outsider_resource_ids: Vec<Uuid> = (0..3).map(|i| Uuid::from_u128(0x400 + i)).collect();
    memberships.extend(outsider_resource_ids.iter().enumerate().map(|(i, res)| {
        ResourceGroupMembership {
            group_id: Uuid::from_u128(0x900 + i as u128),
            resource_type: "gts.cf.core.resources.test.v1~".to_owned(),
            resource_id: res.to_string(),
        }
    }));

    let rg = Arc::new(InMemoryResourceGroupClient::default());
    rg.add_group_descendants(root_group, descendants);
    rg.add_memberships(memberships);
    rg.set_page_size(2); // force multi-page responses

    let upstream = upstream_with_rg(rg);

    let GroupSubtreeFetch {
        resource_ids,
        owner_tenant_ids,
    } = upstream
        .group_subtree(&[root_group])
        .await
        .expect("group subtree resolves");

    let mut expected: Vec<Uuid> = (0..5).map(|i| Uuid::from_u128(0x300 + i)).collect();
    expected.sort();
    let mut got = resource_ids.clone();
    got.sort();
    // Set equality, not a bare count: this asserts BOTH halves at once — every
    // page was drained (all 5 present) AND the drain was narrowed to the granted
    // subtree (none of the out-of-subtree decoys present).
    assert_eq!(
        got, expected,
        "drain must cover every page AND stay narrowed to the granted subtree"
    );
    for outsider in &outsider_resource_ids {
        assert!(
            !resource_ids.contains(outsider),
            "resource {outsider} lives outside the granted subtree and must not be materialized"
        );
    }
    assert_eq!(
        owner_tenant_ids,
        vec![tenant],
        "owning tenant captured from the (fully drained) descendant pages"
    );
}

// ---------- #1b: a cursor walk that cannot terminate must deny, not spin ----------
//
// The accumulated-item cap cannot end these walks: a page that carries a cursor
// but serves no items never moves the counter. Both drains therefore need a page
// budget and a non-advancing-cursor check, and both must fail CLOSED — these
// build an allow-set, so returning the rows collected so far would silently
// narrow it, and looping would hold the cache's single-flight lease forever.
//
// Each test asserts *which* guard fired, via the call count: catching a repeated
// cursor costs two pages, while endless fresh cursors have to exhaust the budget.
// Asserting only `is_err()` would pass if the wrong guard (or a coincidental
// upstream error) ended the walk.

/// Build the adapter over `rg` with the tenant resolver inert.
///
/// No cache and no `HierarchyClient`: these tests are about the drains, and
/// going through the cache would let a single-flight hit mask a walk that never
/// ran.
fn upstream_with_rg(rg: Arc<InMemoryResourceGroupClient>) -> SdkHierarchyUpstream {
    let tr: Arc<dyn TenantResolverClient> =
        Arc::new(InMemoryTenantResolverClient::with_tenants(vec![]));
    SdkHierarchyUpstream::new(tr, rg, Arc::new(AuthZMetrics::from_global()))
}

#[tokio::test]
async fn group_descendants_drain_denies_a_non_advancing_cursor() {
    let rg = Arc::new(InMemoryResourceGroupClient::default());
    rg.set_stuck_cursor(StuckTarget::Descendants, StuckCursor::RepeatedCursor);
    let upstream = upstream_with_rg(Arc::clone(&rg));

    match upstream.group_subtree(&[Uuid::from_u128(0x100)]).await {
        Err(PluginError::Internal { detail: msg }) => assert!(
            msg.contains("non-advancing cursor"),
            "expected the repeated-cursor guard to fire, got {msg:?}"
        ),
        other => panic!("expected a fail-closed error, got {other:?}"),
    }
    assert_eq!(
        rg.call_count(),
        2,
        "the repetition is visible on the second page, so the walk must stop there \
         rather than run out the page budget"
    );
}

#[tokio::test]
async fn group_descendants_drain_denies_endless_empty_pages() {
    let rg = Arc::new(InMemoryResourceGroupClient::default());
    rg.set_stuck_cursor(StuckTarget::Descendants, StuckCursor::EndlessEmptyPages);
    let upstream = upstream_with_rg(Arc::clone(&rg));

    match upstream.group_subtree(&[Uuid::from_u128(0x100)]).await {
        Err(PluginError::Internal { detail: msg }) => assert!(
            msg.contains("page cap"),
            "expected the page budget to fire (no cursor repeats and no items \
             accumulate, so no other guard can), got {msg:?}"
        ),
        other => panic!("expected a fail-closed error, got {other:?}"),
    }
    assert_eq!(
        rg.call_count(),
        MAX_PAGINATED_PAGES,
        "the walk must stop at exactly the page budget"
    );
}

#[tokio::test]
async fn memberships_drain_denies_a_non_advancing_cursor() {
    let rg = Arc::new(InMemoryResourceGroupClient::default());
    // Descendants answer normally (one empty page, no cursor), so the subtree is
    // the root alone and the walk under test is the membership one.
    rg.set_stuck_cursor(StuckTarget::Memberships, StuckCursor::RepeatedCursor);
    let upstream = upstream_with_rg(Arc::clone(&rg));

    match upstream.group_subtree(&[Uuid::from_u128(0x100)]).await {
        Err(PluginError::Internal { detail: msg }) => assert!(
            msg.contains("non-advancing cursor"),
            "expected the repeated-cursor guard to fire, got {msg:?}"
        ),
        other => panic!("expected a fail-closed error, got {other:?}"),
    }
    assert_eq!(
        rg.call_count(),
        3,
        "one descendants page plus the two membership pages the repetition needs"
    );
}

#[tokio::test]
async fn memberships_drain_denies_endless_empty_pages() {
    let rg = Arc::new(InMemoryResourceGroupClient::default());
    rg.set_stuck_cursor(StuckTarget::Memberships, StuckCursor::EndlessEmptyPages);
    let upstream = upstream_with_rg(Arc::clone(&rg));

    match upstream.group_subtree(&[Uuid::from_u128(0x100)]).await {
        Err(PluginError::Internal { detail: msg }) => assert!(
            msg.contains("page cap"),
            "expected the page budget to fire, got {msg:?}"
        ),
        other => panic!("expected a fail-closed error, got {other:?}"),
    }
    assert_eq!(
        rg.call_count(),
        MAX_PAGINATED_PAGES + 1,
        "one descendants page plus the full membership page budget"
    );
}

// ---------- #7: deterministic-vs-transient error mapping ----------
//
// These exercise the two private mapping helpers directly (the test module is
// a child of the adapter, so it can see them). The contract: a
// DETERMINISTIC rejection (tenant/group missing or not accessible) must carry
// the distinct *NotFound VARIANT, which carries a `scope_unresolvable` reason,
// so the metrics classifier does not page it as a transient `resolver_timeout`.
// Everything else stays the *Unavailable transient-outage variant.
mod error_mapping {
    use super::super::{map_resource_group_error, map_tenant_resolver_error};
    use crate::domain::error::PluginError;
    use tenant_resolver_sdk::TenantResolverError;
    use tenant_resolver_sdk::models::TenantId;
    use toolkit_canonical_errors::{CanonicalError, resource_error};
    use uuid::Uuid;

    // Group-scoped resource marker for building canonical errors under test.
    #[resource_error("gts.cf.core.resource_group.group.v1~")]
    struct RgScope;

    #[test]
    fn tenant_not_found_maps_to_deterministic_message() {
        let err = TenantResolverError::TenantNotFound {
            tenant_id: TenantId(Uuid::from_u128(1)),
        };
        assert_eq!(map_tenant_resolver_error(&err), PluginError::TenantNotFound);
    }

    #[test]
    fn tenant_unauthorized_maps_to_deterministic_message() {
        // The TR SDK reserves `Unauthorized`; built-in plugins overload
        // `TenantNotFound` for it today, but if a plugin ever returns it we
        // still treat it as deterministic, not a transient outage.
        let err = TenantResolverError::Unauthorized;
        assert_eq!(map_tenant_resolver_error(&err), PluginError::TenantNotFound);
    }

    #[test]
    fn tenant_service_unavailable_stays_transient() {
        let err = TenantResolverError::ServiceUnavailable("upstream down".to_owned());
        assert_eq!(
            map_tenant_resolver_error(&err),
            PluginError::TenantResolverUnavailable
        );
    }

    #[test]
    fn tenant_internal_stays_transient() {
        let err = TenantResolverError::Internal("boom".to_owned());
        assert_eq!(
            map_tenant_resolver_error(&err),
            PluginError::TenantResolverUnavailable
        );
    }

    #[test]
    fn rg_not_found_maps_to_deterministic_message() {
        let err = RgScope::not_found("g-1").with_resource("g-1").create();
        assert_eq!(
            map_resource_group_error(&err),
            PluginError::ResourceGroupNotFound
        );
    }

    #[test]
    fn rg_access_denied_maps_to_deterministic_message() {
        let err = RgScope::permission_denied()
            .with_reason("access denied")
            .create();
        assert_eq!(
            map_resource_group_error(&err),
            PluginError::ResourceGroupNotFound
        );
    }

    #[test]
    fn rg_service_unavailable_stays_transient() {
        let err = CanonicalError::service_unavailable()
            .with_detail("upstream down")
            .create();
        assert_eq!(
            map_resource_group_error(&err),
            PluginError::ResourceGroupUnavailable
        );
    }
}
