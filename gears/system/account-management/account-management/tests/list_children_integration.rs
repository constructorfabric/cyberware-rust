//! Real-DB integration tests for `TenantRepoImpl::list_children`
//! exercising the `OData` filter / cursor surface against in-memory
//! `SQLite`.
//!
//! Unit-level coverage lives in
//! `domain::tenant::service::service_tests` and pins service-layer
//! semantics (PEP gate, parent guard, lifter shape) against the
//! `FakeTenantRepo` — that fake honours only the minimal
//! `$filter=status eq <i16>` predicate it needs to assert the
//! hidden-AND default and the explicit override. The repo-level
//! `paginate_odata` machinery (filter AST → `SeaORM` condition,
//! cursor encode / decode, tiebreaker ordering, base-condition
//! composition) is too easy to misroute on the fake; this file lifts
//! those invariants to the real `SeaORM` path:
//!
//! * Hidden-AND default — empty `$filter` excludes `Deleted`.
//! * Explicit `$filter=status eq 'deleted'` returns Deleted rows.
//! * `$filter=tenant_type_uuid eq <uuid>` partitions a mixed-type
//!   sibling set.
//! * `$filter=self_managed eq true` isolates the barrier children.
//! * Cursor round-trip — `limit=2` over 5 active siblings walks the
//!   full set across three page reads, observing exactly the inputs
//!   the first page suggests.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use account_management::domain::tenant::TenantRepo;
use account_management_sdk::TenantInfoFilterField;
use sea_orm::ActiveValue;
use time::{Duration, OffsetDateTime};
use toolkit_odata::ast::{CompareOperator, Expr, Value as OdataValue};
use toolkit_odata::filter::FilterField;
use toolkit_odata::{CursorV1, ODataOrderBy, ODataQuery, OrderKey, SortDir};
use toolkit_security::{
    AccessScope, InTenantSubtreeScopeFilter, ScopeConstraint, ScopeFilter, pep_properties,
};
use uuid::Uuid;

use account_management::infra::storage::entity::tenants;
use common::*;

// ---- helpers ---------------------------------------------------------

const ROOT_ID: u128 = 0x100;

fn ts_at(secs: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000 + secs).expect("epoch + offset")
}

/// Seed a tenant with explicit `created_at` so `(created_at ASC, id ASC)`
/// ordering stays deterministic across the cursor round-trip. The
/// shared `common::insert_tenant` helper uses `now_utc()` which can
/// collide at microsecond precision on fast sequential inserts.
async fn seed_tenant_at(
    h: &Harness,
    id: Uuid,
    parent_id: Uuid,
    status_smallint: i16,
    self_managed: bool,
    tenant_type_uuid: Uuid,
    created_at: OffsetDateTime,
) {
    use toolkit_db::secure::secure_insert;
    let conn = h.provider.conn().expect("conn");
    let am = tenants::ActiveModel {
        id: ActiveValue::Set(id),
        parent_id: ActiveValue::Set(Some(parent_id)),
        name: ActiveValue::Set(format!("t-{id}")),
        status: ActiveValue::Set(status_smallint),
        self_managed: ActiveValue::Set(self_managed),
        tenant_type_uuid: ActiveValue::Set(tenant_type_uuid),
        depth: ActiveValue::Set(1),
        created_at: ActiveValue::Set(created_at),
        updated_at: ActiveValue::Set(created_at),
        deleted_at: ActiveValue::Set(if status_smallint == DELETED {
            Some(created_at)
        } else {
            None
        }),
        retention_window_secs: ActiveValue::Set(None),
        claimed_by: ActiveValue::Set(None),
        claimed_at: ActiveValue::Set(None),
        terminal_failure_at: ActiveValue::Set(None),
    };
    secure_insert::<tenants::Entity>(am, &allow_all(), &conn)
        .await
        .expect("seed child");
}

async fn seed_root(h: &Harness, root_id: Uuid) {
    insert_tenant(&h.provider, root_id, None, "root", ACTIVE, false, 0)
        .await
        .expect("seed root");
    insert_closure(&h.provider, root_id, root_id, 0, ACTIVE)
        .await
        .expect("root self-row");
}

/// Seed an active, non-barrier child with an explicit `name` so the
/// name-filter / name-orderby tests can assert on a controlled label
/// (the generic `seed_tenant_at` derives `name` from the id, which is
/// not lexicographically controllable).
async fn seed_tenant_named(
    h: &Harness,
    id: Uuid,
    parent_id: Uuid,
    name: &str,
    tenant_type_uuid: Uuid,
    created_at: OffsetDateTime,
) {
    use toolkit_db::secure::secure_insert;
    let conn = h.provider.conn().expect("conn");
    let am = tenants::ActiveModel {
        id: ActiveValue::Set(id),
        parent_id: ActiveValue::Set(Some(parent_id)),
        name: ActiveValue::Set(name.to_owned()),
        status: ActiveValue::Set(ACTIVE),
        self_managed: ActiveValue::Set(false),
        tenant_type_uuid: ActiveValue::Set(tenant_type_uuid),
        depth: ActiveValue::Set(1),
        created_at: ActiveValue::Set(created_at),
        updated_at: ActiveValue::Set(created_at),
        deleted_at: ActiveValue::Set(None),
        retention_window_secs: ActiveValue::Set(None),
        claimed_by: ActiveValue::Set(None),
        claimed_at: ActiveValue::Set(None),
        terminal_failure_at: ActiveValue::Set(None),
    };
    secure_insert::<tenants::Entity>(am, &allow_all(), &conn)
        .await
        .expect("seed named child");
}

/// Build `$filter=<field> eq <i16>` for a numeric column. Used by
/// non-status numeric columns in these tests (none currently — kept
/// as scaffold for future numeric filters).
#[allow(dead_code)]
fn filter_field_eq_i64(field: &str, value: i64) -> Expr {
    Expr::Compare(
        Box::new(Expr::Identifier(field.to_owned())),
        CompareOperator::Eq,
        Box::new(Expr::Value(OdataValue::Number(value.into()))),
    )
}

/// Build `$filter=<field> eq '<label>'` for a string-encoded column
/// (e.g. the `status` lifecycle enum exposed as a string contract on
/// the SDK surface).
fn filter_field_eq_string(field: &str, label: &str) -> Expr {
    Expr::Compare(
        Box::new(Expr::Identifier(field.to_owned())),
        CompareOperator::Eq,
        Box::new(Expr::Value(OdataValue::String(label.to_owned()))),
    )
}

fn filter_field_eq_uuid(field: &str, value: Uuid) -> Expr {
    Expr::Compare(
        Box::new(Expr::Identifier(field.to_owned())),
        CompareOperator::Eq,
        Box::new(Expr::Value(OdataValue::Uuid(value))),
    )
}

fn filter_field_eq_bool(field: &str, value: bool) -> Expr {
    Expr::Compare(
        Box::new(Expr::Identifier(field.to_owned())),
        CompareOperator::Eq,
        Box::new(Expr::Value(OdataValue::Bool(value))),
    )
}

fn ids_of(items: &[account_management::domain::tenant::model::TenantModel]) -> Vec<Uuid> {
    items.iter().map(|t| t.id).collect()
}

// ---- pinned filter-field surface ------------------------------------

/// Sanity-pin against the SDK's declared filter set — duplicated from
/// the SDK-side `tenant_filter_fields_are_pinned` so the integration
/// suite trips immediately on a wire-contract drift even when the SDK
/// lib tests are not re-run.
#[test]
fn tenant_filter_fields_are_stable() {
    let names: Vec<&'static str> = TenantInfoFilterField::FIELDS
        .iter()
        .map(FilterField::name)
        .collect();
    assert_eq!(
        names,
        vec![
            "id",
            "name",
            "status",
            "tenant_type_uuid",
            "tenant_type",
            "self_managed",
            "created_at",
            "updated_at",
        ],
    );
}

// ---- hidden-AND status default --------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_children_default_filter_hides_deleted() {
    let h = setup_sqlite().await.expect("harness");
    let root = Uuid::from_u128(ROOT_ID);
    seed_root(&h, root).await;

    let active = Uuid::from_u128(0x201);
    let suspended = Uuid::from_u128(0x202);
    let deleted = Uuid::from_u128(0x203);
    let type_a = Uuid::from_u128(0xAA);
    seed_tenant_at(&h, active, root, ACTIVE, false, type_a, ts_at(1)).await;
    seed_tenant_at(&h, suspended, root, SUSPENDED, false, type_a, ts_at(2)).await;
    seed_tenant_at(&h, deleted, root, DELETED, false, type_a, ts_at(3)).await;

    let page = h
        .repo
        .list_children(&allow_all(), root, &ODataQuery::default())
        .await
        .expect("list");

    assert_eq!(
        ids_of(&page.items),
        vec![active, suspended],
        "empty `$filter` MUST drop Deleted rows via the repo-level \
         hidden-AND default"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_children_explicit_status_filter_returns_deleted() {
    let h = setup_sqlite().await.expect("harness");
    let root = Uuid::from_u128(ROOT_ID);
    seed_root(&h, root).await;

    let active = Uuid::from_u128(0x201);
    let deleted = Uuid::from_u128(0x202);
    let type_a = Uuid::from_u128(0xAA);
    seed_tenant_at(&h, active, root, ACTIVE, false, type_a, ts_at(1)).await;
    seed_tenant_at(&h, deleted, root, DELETED, false, type_a, ts_at(2)).await;

    let query = ODataQuery::default().with_filter(filter_field_eq_string("status", "deleted"));
    let page = h
        .repo
        .list_children(&allow_all(), root, &query)
        .await
        .expect("list");

    assert_eq!(
        ids_of(&page.items),
        vec![deleted],
        "`$filter=status eq 'deleted'` MUST bypass the hidden-AND \
         default and return the soft-deleted row"
    );
}

// ---- grouped direct-child counts ------------------------------------

/// `count_children_grouped` powers the public `child_count` field. It
/// tallies *direct* children per parent in one grouped query, and the
/// public-surface semantics differ from the delete-saga
/// `count_children` guard: `Provisioning` is **excluded**, `Deleted`
/// is **included**, and parents with no matching child are simply
/// absent from the map (callers default them to `0`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn count_children_grouped_excludes_provisioning_includes_deleted() {
    let h = setup_sqlite().await.expect("harness");
    let root = Uuid::from_u128(ROOT_ID);
    seed_root(&h, root).await;

    let type_a = Uuid::from_u128(0xAA);
    // Two direct children of root; `child_a` is a branch, `child_b` a leaf.
    let child_a = Uuid::from_u128(0x201);
    let child_b = Uuid::from_u128(0x202);
    seed_tenant_at(&h, child_a, root, ACTIVE, false, type_a, ts_at(1)).await;
    seed_tenant_at(&h, child_b, root, ACTIVE, false, type_a, ts_at(2)).await;

    // Grandchildren under `child_a`: active + suspended + deleted are
    // all counted (3); the provisioning row is excluded.
    seed_tenant_at(
        &h,
        Uuid::from_u128(0x301),
        child_a,
        ACTIVE,
        false,
        type_a,
        ts_at(3),
    )
    .await;
    seed_tenant_at(
        &h,
        Uuid::from_u128(0x302),
        child_a,
        SUSPENDED,
        false,
        type_a,
        ts_at(4),
    )
    .await;
    seed_tenant_at(
        &h,
        Uuid::from_u128(0x303),
        child_a,
        DELETED,
        false,
        type_a,
        ts_at(5),
    )
    .await;
    seed_tenant_at(
        &h,
        Uuid::from_u128(0x304),
        child_a,
        PROVISIONING,
        false,
        type_a,
        ts_at(6),
    )
    .await;

    let counts = h
        .repo
        .count_children_grouped(&allow_all(), &[child_a, child_b])
        .await
        .expect("grouped count");

    assert_eq!(
        counts.get(&child_a).copied(),
        Some(3),
        "child_a: active + suspended + deleted counted; provisioning excluded"
    );
    assert_eq!(
        counts.get(&child_b).copied(),
        None,
        "child_b has no children -> absent from the map (caller defaults to 0)"
    );
}

/// Build a Respect-barriers `InTenantSubtree` scope rooted at `root`,
/// keyed on the tenant row's own id — the exact shape AM's PDP emits
/// for a tenant read (`tenants` maps `resource_col = "id"`).
fn respect_scope_rooted_at(root: Uuid) -> AccessScope {
    AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::InTenantSubtree(
        InTenantSubtreeScopeFilter::new(pep_properties::RESOURCE_ID, root),
    )]))
}

/// Regression: a Respect-scoped caller counting a *reachable* parent's
/// direct children MUST include a `self_managed` direct child.
///
/// A `self_managed` tenant's inbound closure edges carry `barrier = 1`
/// even from its own parent, so clamping each child by the caller's
/// barrier-respecting scope silently dropped the `self_managed` direct
/// child from `child_count` — while `list_children` still surfaced it
/// (the direct-child carve-out). The count must gate on the PARENT's
/// reachability, then count all direct children. The existing
/// `count_children_grouped_excludes_provisioning_includes_deleted`
/// test runs under `allow_all` (barriers off) and cannot catch this.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn count_children_grouped_respect_scope_counts_self_managed_direct_child() {
    let h = setup_sqlite().await.expect("harness");
    let root = Uuid::from_u128(ROOT_ID);
    seed_root(&h, root).await;
    let type_a = Uuid::from_u128(0xAA);

    // P: an ordinary (non-barrier) direct child of root — Respect-reachable.
    let p = Uuid::from_u128(0x401);
    seed_tenant_at(&h, p, root, ACTIVE, false, type_a, ts_at(1)).await;
    insert_closure(&h.provider, root, p, 0, ACTIVE)
        .await
        .expect("(root,P) barrier 0");
    insert_closure(&h.provider, p, p, 0, ACTIVE)
        .await
        .expect("(P,P) self-row");

    // c1: ordinary active direct child of P.
    let c1 = Uuid::from_u128(0x402);
    seed_tenant_at(&h, c1, p, ACTIVE, false, type_a, ts_at(2)).await;
    insert_closure(&h.provider, root, c1, 0, ACTIVE)
        .await
        .expect("(root,c1)");
    insert_closure(&h.provider, p, c1, 0, ACTIVE)
        .await
        .expect("(P,c1)");
    insert_closure(&h.provider, c1, c1, 0, ACTIVE)
        .await
        .expect("(c1,c1)");

    // c2: self_managed direct child of P — inbound edges barrier = 1
    // (even from P), self-row barrier = 0. This is the row the old
    // barrier clamp dropped from P's count.
    let c2 = Uuid::from_u128(0x403);
    seed_tenant_at(&h, c2, p, ACTIVE, true, type_a, ts_at(3)).await;
    insert_closure(&h.provider, root, c2, 1, ACTIVE)
        .await
        .expect("(root,c2) barrier 1");
    insert_closure(&h.provider, p, c2, 1, ACTIVE)
        .await
        .expect("(P,c2) barrier 1");
    insert_closure(&h.provider, c2, c2, 0, ACTIVE)
        .await
        .expect("(c2,c2) self-row");

    let scope = respect_scope_rooted_at(root);
    let counts = h
        .repo
        .count_children_grouped(&scope, &[p])
        .await
        .expect("grouped count");
    assert_eq!(
        counts.get(&p).copied(),
        Some(2),
        "P is Respect-reachable, so its child_count must include the \
         self_managed direct child c2 (active c1 + self_managed c2 = 2), \
         not drop c2 to 1 under the barrier clamp"
    );
}

/// The parent gate must not leak a subtree below a barrier: counting the
/// children of a `self_managed` tenant the caller can only reach past
/// the barrier collapses to `0` (absent from the map).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn count_children_grouped_respect_scope_hides_past_barrier_subtree() {
    let h = setup_sqlite().await.expect("harness");
    let root = Uuid::from_u128(ROOT_ID);
    seed_root(&h, root).await;
    let type_a = Uuid::from_u128(0xAA);

    // s: self_managed tenant directly under root — reachable to the
    // caller only via the identity-level carve-out, NOT under Respect.
    let s = Uuid::from_u128(0x411);
    seed_tenant_at(&h, s, root, ACTIVE, true, type_a, ts_at(1)).await;
    insert_closure(&h.provider, root, s, 1, ACTIVE)
        .await
        .expect("(root,s) barrier 1");
    insert_closure(&h.provider, s, s, 0, ACTIVE)
        .await
        .expect("(s,s) self-row");

    // gc: a child of s, strictly below s's barrier.
    let gc = Uuid::from_u128(0x412);
    seed_tenant_at(&h, gc, s, ACTIVE, false, type_a, ts_at(2)).await;
    insert_closure(&h.provider, root, gc, 1, ACTIVE)
        .await
        .expect("(root,gc) barrier 1");
    insert_closure(&h.provider, s, gc, 0, ACTIVE)
        .await
        .expect("(s,gc)");
    insert_closure(&h.provider, gc, gc, 0, ACTIVE)
        .await
        .expect("(gc,gc)");

    let scope = respect_scope_rooted_at(root);
    let counts = h
        .repo
        .count_children_grouped(&scope, &[s])
        .await
        .expect("grouped count");
    assert_eq!(
        counts.get(&s).copied(),
        None,
        "s is not Respect-reachable (past its own barrier from root), so \
         its subtree must not be counted - child_count collapses to 0"
    );
}

/// Empty input short-circuits to an empty map (no query, no panic on an
/// empty `IN ()` predicate).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn count_children_grouped_empty_input_is_empty() {
    let h = setup_sqlite().await.expect("harness");
    let counts = h
        .repo
        .count_children_grouped(&allow_all(), &[])
        .await
        .expect("grouped count");
    assert!(counts.is_empty(), "empty parent set -> empty map");
}

/// `status` filter values outside the public SDK contract — including
/// the AM-internal `'provisioning'` — surface as a validation error
/// from `TenantODataMapper::map_value` before the predicate reaches
/// `SeaORM`. The storage SMALLINT encoding is intentionally NOT part
/// of the wire contract, so numeric forms (`status eq 3`) are also
/// rejected by the framework's kind-validation step before this hook
/// even runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_children_rejects_unknown_status_value() {
    let h = setup_sqlite().await.expect("harness");
    let root = Uuid::from_u128(ROOT_ID);
    seed_root(&h, root).await;

    let query = ODataQuery::default().with_filter(filter_field_eq_string("status", "wat"));
    let err = h
        .repo
        .list_children(&allow_all(), root, &query)
        .await
        .expect_err("unknown status value MUST be rejected");
    let detail = format!("{err:?}");
    assert!(
        detail.contains("invalid `status` value") || detail.contains("invalid status"),
        "expected validation error referencing the invalid status; got {detail}"
    );

    // `'provisioning'` is the AM-internal status and has no SDK
    // representation; the map_value hook MUST reject it the same way
    // as any other unknown string.
    let query = ODataQuery::default().with_filter(filter_field_eq_string("status", "provisioning"));
    let err = h
        .repo
        .list_children(&allow_all(), root, &query)
        .await
        .expect_err("'provisioning' MUST be rejected (internal status)");
    let detail = format!("{err:?}");
    assert!(
        detail.contains("invalid `status` value"),
        "expected validation error referencing the invalid status; got {detail}"
    );
}

/// `status` is a categorical lifecycle column. Ordered operators
/// (`lt`/`le`/`gt`/`ge`) on the wire string would silently fall
/// back to the hidden storage ordinal (`status < 3` meaning "any
/// SMALLINT less than `Deleted`"), which is a confusing semantic
/// mismatch with the public string contract. `toolkit-odata` refuses
/// an ordered operator on a string field before the query reaches the
/// AM mapper, which rejects it as well via `FieldToColumn::map_value`;
/// either way the caller gets a validation error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_children_rejects_ordered_comparison_on_status() {
    let h = setup_sqlite().await.expect("harness");
    let root = Uuid::from_u128(ROOT_ID);
    seed_root(&h, root).await;

    let lt = Expr::Compare(
        Box::new(Expr::Identifier("status".to_owned())),
        CompareOperator::Lt,
        Box::new(Expr::Value(OdataValue::String("deleted".to_owned()))),
    );
    let query = ODataQuery::default().with_filter(lt);
    let err = h
        .repo
        .list_children(&allow_all(), root, &query)
        .await
        .expect_err("ordered comparison on status MUST be rejected");
    let detail = format!("{err:?}");
    assert!(
        detail.contains("not supported on `status`")
            || detail.contains("ordered")
            || detail.contains("Unsupported operation: `lt` on field `status`"),
        "expected the ordered operator to be rejected; got {detail}"
    );
}

/// `$orderby=status` sorts children by the lifecycle ordinal
/// (`Active = 1 < Suspended = 2 < Deleted = 3`), the order the
/// Tenants-page Status column uses. Insertion (`created_at`) order is
/// deliberately NOT the ordinal order, so a passing assertion proves
/// the sort honoured `status`, not the chronological default.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_children_orderby_status_sorts_by_lifecycle_ordinal() {
    let h = setup_sqlite().await.expect("harness");
    let root = Uuid::from_u128(ROOT_ID);
    seed_root(&h, root).await;

    let type_a = Uuid::from_u128(0xAA);
    let suspended_old = Uuid::from_u128(0x701);
    let active_new = Uuid::from_u128(0x702);
    let suspended_new = Uuid::from_u128(0x703);
    seed_tenant_at(&h, suspended_old, root, SUSPENDED, false, type_a, ts_at(1)).await;
    seed_tenant_at(&h, active_new, root, ACTIVE, false, type_a, ts_at(2)).await;
    seed_tenant_at(&h, suspended_new, root, SUSPENDED, false, type_a, ts_at(3)).await;

    let query = ODataQuery::default().with_order(ODataOrderBy(vec![OrderKey {
        field: "status".to_owned(),
        dir: SortDir::Asc,
    }]));
    let page = h
        .repo
        .list_children(&allow_all(), root, &query)
        .await
        .expect("$orderby=status must be accepted");

    // Effective order is (status ASC, id ASC): active first, then the
    // two suspended rows tie-broken by id.
    assert_eq!(
        ids_of(&page.items),
        vec![active_new, suspended_old, suspended_new],
        "`$orderby=status asc` MUST sort by lifecycle ordinal with id tiebreak"
    );
}

/// Cursor guard: ordering by `status` must survive the cursor
/// round-trip. The cursor token carries the storage ordinal
/// (`cursor_kind = I64` on the AM mapper), so page 2+ predicates
/// compare SMALLINT-to-integer — the exact wire-vs-storage mismatch
/// that used to force `is_orderable = false` for this field.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_children_orderby_status_cursor_roundtrip() {
    let h = setup_sqlite().await.expect("harness");
    let root = Uuid::from_u128(ROOT_ID);
    seed_root(&h, root).await;

    let type_a = Uuid::from_u128(0xAA);
    // 5 children, statuses interleaved so every page boundary lands
    // inside or between ordinal groups.
    let seeds: Vec<(Uuid, i16)> = vec![
        (Uuid::from_u128(0x711), SUSPENDED),
        (Uuid::from_u128(0x712), ACTIVE),
        (Uuid::from_u128(0x713), SUSPENDED),
        (Uuid::from_u128(0x714), ACTIVE),
        (Uuid::from_u128(0x715), ACTIVE),
    ];
    for (i, (id, status)) in seeds.iter().enumerate() {
        seed_tenant_at(
            &h,
            *id,
            root,
            *status,
            false,
            type_a,
            ts_at(1) + Duration::seconds(i64::try_from(i).unwrap()),
        )
        .await;
    }

    // Expected effective order (status ASC, id ASC): the three active
    // rows by id, then the two suspended rows by id.
    let expected = vec![
        Uuid::from_u128(0x712),
        Uuid::from_u128(0x714),
        Uuid::from_u128(0x715),
        Uuid::from_u128(0x711),
        Uuid::from_u128(0x713),
    ];

    // Bound the walk: a mapper/codec regression that emits a repeated
    // `next_cursor` would otherwise loop forever instead of failing. A
    // cursor seen twice is a hard assertion, not a hang.
    let mut seen_cursors = std::collections::HashSet::new();
    let mut collected = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut q = ODataQuery::default().with_limit(2);
        if let Some(c) = cursor.take() {
            assert!(
                seen_cursors.insert(c.clone()),
                "cursor walk returned a repeated cursor - pagination is not advancing"
            );
            q = q.with_cursor(CursorV1::decode(&c).expect("decode status cursor"));
        } else {
            q = q.with_order(ODataOrderBy(vec![OrderKey {
                field: "status".to_owned(),
                dir: SortDir::Asc,
            }]));
        }
        let page = h
            .repo
            .list_children(&allow_all(), root, &q)
            .await
            .expect("status-ordered cursor page must not fail");
        collected.extend(ids_of(&page.items));
        match page.page_info.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(
        collected, expected,
        "cursor walk over `$orderby=status` MUST visit every row exactly \
         once in (status ASC, id ASC) order"
    );
}

// ---- typed filters --------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_children_filter_tenant_type_uuid_partitions_set() {
    let h = setup_sqlite().await.expect("harness");
    let root = Uuid::from_u128(ROOT_ID);
    seed_root(&h, root).await;

    let type_a = Uuid::from_u128(0xAA);
    let type_b = Uuid::from_u128(0xBB);
    let a1 = Uuid::from_u128(0x301);
    let a2 = Uuid::from_u128(0x302);
    let b1 = Uuid::from_u128(0x303);
    let b2 = Uuid::from_u128(0x304);
    seed_tenant_at(&h, a1, root, ACTIVE, false, type_a, ts_at(1)).await;
    seed_tenant_at(&h, a2, root, ACTIVE, false, type_a, ts_at(2)).await;
    seed_tenant_at(&h, b1, root, ACTIVE, false, type_b, ts_at(3)).await;
    seed_tenant_at(&h, b2, root, ACTIVE, false, type_b, ts_at(4)).await;

    let query = ODataQuery::default().with_filter(filter_field_eq_uuid("tenant_type_uuid", type_a));
    let page = h
        .repo
        .list_children(&allow_all(), root, &query)
        .await
        .expect("list");

    assert_eq!(
        ids_of(&page.items),
        vec![a1, a2],
        "`$filter=tenant_type_uuid eq <type-A>` MUST partition the set \
         to type-A rows only"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_children_filter_self_managed_isolates_barrier_children() {
    let h = setup_sqlite().await.expect("harness");
    let root = Uuid::from_u128(ROOT_ID);
    seed_root(&h, root).await;

    let type_a = Uuid::from_u128(0xAA);
    let plain1 = Uuid::from_u128(0x401);
    let plain2 = Uuid::from_u128(0x402);
    let boundary = Uuid::from_u128(0x403);
    seed_tenant_at(&h, plain1, root, ACTIVE, false, type_a, ts_at(1)).await;
    seed_tenant_at(&h, plain2, root, ACTIVE, false, type_a, ts_at(2)).await;
    seed_tenant_at(&h, boundary, root, ACTIVE, true, type_a, ts_at(3)).await;

    let query = ODataQuery::default().with_filter(filter_field_eq_bool("self_managed", true));
    let page = h
        .repo
        .list_children(&allow_all(), root, &query)
        .await
        .expect("list");

    assert_eq!(
        ids_of(&page.items),
        vec![boundary],
        "`$filter=self_managed eq true` MUST return only the barrier child"
    );
}

// ---- name filter / orderby ------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_children_filter_by_name_returns_only_match() {
    let h = setup_sqlite().await.expect("harness");
    let root = Uuid::from_u128(ROOT_ID);
    seed_root(&h, root).await;

    let type_a = Uuid::from_u128(0xAA);
    let alpha = Uuid::from_u128(0x501);
    let beta = Uuid::from_u128(0x502);
    seed_tenant_named(&h, alpha, root, "alpha", type_a, ts_at(1)).await;
    seed_tenant_named(&h, beta, root, "beta", type_a, ts_at(2)).await;

    let query = ODataQuery::default().with_filter(filter_field_eq_string("name", "alpha"));
    let page = h
        .repo
        .list_children(&allow_all(), root, &query)
        .await
        .expect("list");

    assert_eq!(
        ids_of(&page.items),
        vec![alpha],
        "`$filter=name eq 'alpha'` MUST return only the exactly-named child"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_children_orderby_name_sorts_lexicographically() {
    let h = setup_sqlite().await.expect("harness");
    let root = Uuid::from_u128(ROOT_ID);
    seed_root(&h, root).await;

    let type_a = Uuid::from_u128(0xAA);
    let charlie = Uuid::from_u128(0x601);
    let alpha = Uuid::from_u128(0x602);
    let bravo = Uuid::from_u128(0x603);
    // Insertion (created_at) order is charlie, alpha, bravo — deliberately
    // NOT the lexicographic name order, so a passing assertion proves the
    // sort honoured `name`, not the default `created_at`.
    seed_tenant_named(&h, charlie, root, "charlie", type_a, ts_at(1)).await;
    seed_tenant_named(&h, alpha, root, "alpha", type_a, ts_at(2)).await;
    seed_tenant_named(&h, bravo, root, "bravo", type_a, ts_at(3)).await;

    let query = ODataQuery::default().with_order(ODataOrderBy(vec![OrderKey {
        field: "name".to_owned(),
        dir: SortDir::Asc,
    }]));
    let page = h
        .repo
        .list_children(&allow_all(), root, &query)
        .await
        .expect("list");

    assert_eq!(
        ids_of(&page.items),
        vec![alpha, bravo, charlie],
        "`$orderby=name asc` MUST sort children lexicographically by name"
    );
}

// ---- cursor pagination ----------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_children_cursor_pagination_walks_full_set() {
    let h = setup_sqlite().await.expect("harness");
    let root = Uuid::from_u128(ROOT_ID);
    seed_root(&h, root).await;

    let type_a = Uuid::from_u128(0xAA);
    // Stamp each child with a strictly-increasing `created_at` so the
    // tiebreaker order (`created_at ASC`) is fully deterministic — at
    // microsecond resolution sequential inserts can collide.
    let ids: Vec<Uuid> = (0..5u128).map(|i| Uuid::from_u128(0x500 + i)).collect();
    for (i, id) in ids.iter().enumerate() {
        seed_tenant_at(
            &h,
            *id,
            root,
            ACTIVE,
            false,
            type_a,
            ts_at(1) + Duration::seconds(i64::try_from(i).unwrap()),
        )
        .await;
    }

    // First page — limit=2, no cursor: returns ids 0..2, must carry
    // `next_cursor` because there are 3 more rows.
    let first = h
        .repo
        .list_children(&allow_all(), root, &ODataQuery::default().with_limit(2))
        .await
        .expect("first");
    assert_eq!(ids_of(&first.items), vec![ids[0], ids[1]]);
    let cursor1 = first
        .page_info
        .next_cursor
        .expect("must yield next_cursor when 5 rows > limit=2");
    assert!(first.page_info.prev_cursor.is_none());

    // Second page — feed the cursor; returns ids 2..4 + next_cursor.
    let cv1 = CursorV1::decode(&cursor1).expect("decode cursor1");
    let second = h
        .repo
        .list_children(
            &allow_all(),
            root,
            &ODataQuery::default().with_limit(2).with_cursor(cv1),
        )
        .await
        .expect("second");
    assert_eq!(ids_of(&second.items), vec![ids[2], ids[3]]);
    let cursor2 = second
        .page_info
        .next_cursor
        .expect("must yield next_cursor when row 4 remains");

    // Third page — single row, no further next_cursor.
    let cv2 = CursorV1::decode(&cursor2).expect("decode cursor2");
    let third = h
        .repo
        .list_children(
            &allow_all(),
            root,
            &ODataQuery::default().with_limit(2).with_cursor(cv2),
        )
        .await
        .expect("third");
    assert_eq!(ids_of(&third.items), vec![ids[4]]);
    assert!(
        third.page_info.next_cursor.is_none(),
        "final page MUST NOT carry next_cursor"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_children_cursor_pagination_survives_created_at_collision() {
    // Regression pin for the cursor row-loss bug fixed by switching
    // `paginate_odata`'s tiebreaker from `("created_at", Asc)` (non-
    // unique → effective order is partial) to `("id", Asc)` (PK,
    // total order). When two siblings share a `created_at`
    // timestamp the pre-fix cursor predicate `created_at > last_ts`
    // skipped the collision-mates on the next page.
    //
    // Seed four siblings with **identical** `created_at` (the worst
    // case: 100% collision). Walk the listing with `limit=1`. The
    // composite `(created_at ASC, id ASC)` total order must surface
    // all four rows in deterministic UUID-ascending order across
    // four pages with no row-loss.
    let h = setup_sqlite().await.expect("harness");
    let root = Uuid::from_u128(ROOT_ID);
    seed_root(&h, root).await;

    let type_a = Uuid::from_u128(0xAA);
    // Same `created_at` for every sibling. UUIDs in numeric order so
    // the predicted text-collation order (`00000000-...-000000000600`,
    // `...0601`, `...0602`, `...0603`) matches numeric order.
    let collision_ts = ts_at(1);
    let ids: Vec<Uuid> = (0..4u128).map(|i| Uuid::from_u128(0x600 + i)).collect();
    for id in &ids {
        seed_tenant_at(&h, *id, root, ACTIVE, false, type_a, collision_ts).await;
    }

    // Walk all four pages with limit=1; collect ids returned.
    let mut walked: Vec<Uuid> = Vec::new();
    let mut cursor: Option<CursorV1> = None;
    for page_n in 0..4 {
        let mut q = ODataQuery::default().with_limit(1);
        if let Some(c) = cursor.take() {
            q = q.with_cursor(c);
        }
        let page = h
            .repo
            .list_children(&allow_all(), root, &q)
            .await
            .expect("paged list");
        assert_eq!(
            page.items.len(),
            1,
            "page {page_n} MUST return exactly one row (limit=1, four rows total)"
        );
        walked.push(page.items[0].id);
        cursor = page
            .page_info
            .next_cursor
            .and_then(|s| CursorV1::decode(&s).ok());
    }

    // Total order guarantee: union of walked == seeded set, in
    // deterministic UUID-ascending order. Without the fix the second
    // page would return 0 rows (filter `created_at > last_ts`
    // matches nothing under collision) and `walked.len()` would
    // be 1 — the assertion above would already trip.
    assert_eq!(
        walked, ids,
        "cursor walk over `(created_at ASC, id ASC)` MUST surface every \
         row in UUID-ascending order when `created_at` is identical \
         across siblings"
    );
}
