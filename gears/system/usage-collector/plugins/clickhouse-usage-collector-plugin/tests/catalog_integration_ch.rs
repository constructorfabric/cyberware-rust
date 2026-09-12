#![cfg(feature = "clickhouse")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
//! `ClickHouse`-backed integration tests for [`ChCatalogStore`]
//! (create / get / list / delete).
//!
//! Mirror the `TimescaleDB` reference plugin catalog tests where the contract
//! is shared. `delete` is the one place the two backends reach the same
//! contract by different means: Postgres has a real
//! `ON DELETE RESTRICT` foreign key, while this backend emulates it with a
//! capped pre-delete reference probe and a post-delete orphan sweep. The
//! delete tests here therefore assert the *observable* contract —
//! `UsageTypeNotFound` on absence, `UsageTypeReferenced` on a live reference,
//! removal visible to the next read otherwise — plus the two properties the
//! emulation adds: that the catalog row goes away before the sweep (so the
//! record store starts refusing new records on its own), and that the sweep's
//! mutation is synchronous. The residual race between probe and delete is
//! accepted by design and is not testable from here; see `ChCatalogStore::
//! delete`.
//!
//! Requires Docker for `ClickHouse`.

mod common;

use rust_decimal::Decimal;
use toolkit_odata::ast::{CompareOperator, Expr, Value};
use toolkit_odata::{CursorV1, ODataQuery};
use uuid::Uuid;

use usage_collector_sdk::{UsageCollectorPluginError, UsageKind};

use clickhouse_usage_collector_plugin::domain::ports::{CatalogStore, RecordStore};

const VCPU_GTS: &str = "gts.cf.core.uc.usage_record.v1~cf.compute._.vcpu_hours.v1";
const RAM_GTS: &str = "gts.cf.core.uc.usage_record.v1~cf.compute._.ram_gb.v1";
const MISSING_GTS: &str = "gts.cf.core.uc.usage_record.v1~cf.compute._.absent.v1";
const DISK_GTS: &str = "gts.cf.core.uc.usage_record.v1~cf.compute._.disk_gb.v1";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_create_then_get_roundtrips() {
    let Some(h) = common::bring_up_or_skip().await else {
        return;
    };
    let store = common::catalog_store(&h);

    let ut = common::fixture_usage_type(VCPU_GTS, "counter", &["region", "tier"]);
    let created = store.create(ut.clone()).await.expect("create");
    assert_eq!(created, ut, "create returns the stored usage type");

    let fetched = store
        .get(common::fixture_gts_id(VCPU_GTS))
        .await
        .expect("get");
    assert_eq!(fetched.kind, UsageKind::Counter, "kind roundtrips");
    assert_eq!(
        fetched.metadata_fields, ut.metadata_fields,
        "metadata_fields roundtrip"
    );
}

/// Re-creating the same `gts_id` with a **differing** payload conflicts.
///
/// Only a differing payload conflicts; an identical resubmission is absorbed
/// (see [`ch_create_identical_resubmission_is_absorbed`]).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_create_duplicate_is_already_exists() {
    let Some(h) = common::bring_up_or_skip().await else {
        return;
    };
    let store = common::catalog_store(&h);

    store
        .create(common::fixture_usage_type(VCPU_GTS, "counter", &["region"]))
        .await
        .expect("first create");

    // Differing `kind` *and* `metadata_fields` for the same gts_id.
    let err = store
        .create(common::fixture_usage_type(VCPU_GTS, "gauge", &[]))
        .await
        .expect_err("second create with a differing payload must conflict");
    assert!(
        matches!(
            err,
            UsageCollectorPluginError::UsageTypeAlreadyExists { .. }
        ),
        "differing-payload create must be UsageTypeAlreadyExists, got {err:?}"
    );

    // The stored row is untouched by the rejected create.
    let fetched = store
        .get(common::fixture_gts_id(VCPU_GTS))
        .await
        .expect("get after rejected create");
    assert_eq!(
        fetched.kind,
        UsageKind::Counter,
        "the rejected create must not overwrite the stored kind"
    );
}

/// An identical resubmission is absorbed silently (SPI idempotency rule) and
/// returns the stored usage type rather than `UsageTypeAlreadyExists`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_create_identical_resubmission_is_absorbed() {
    let Some(h) = common::bring_up_or_skip().await else {
        return;
    };
    let store = common::catalog_store(&h);

    let ut = common::fixture_usage_type(VCPU_GTS, "gauge", &["region", "tier"]);
    let first = store.create(ut.clone()).await.expect("first create");
    let second = store
        .create(ut.clone())
        .await
        .expect("identical resubmission must be absorbed, not rejected");

    assert_eq!(first, second, "absorb returns the stored usage type");
    assert_eq!(second.kind, UsageKind::Gauge, "kind survives the absorb");
    assert_eq!(
        second.metadata_fields, ut.metadata_fields,
        "metadata_fields survive the absorb"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_get_missing_is_not_found() {
    let Some(h) = common::bring_up_or_skip().await else {
        return;
    };
    let store = common::catalog_store(&h);

    let err = store
        .get(common::fixture_gts_id(MISSING_GTS))
        .await
        .expect_err("absent get must fail");
    assert!(
        matches!(err, UsageCollectorPluginError::UsageTypeNotFound { .. }),
        "absent get must be UsageTypeNotFound, got {err:?}"
    );
}

/// An unreferenced type deletes cleanly and is gone by the very next read.
///
/// `ALTER TABLE … DELETE` is asynchronous by default, so the `get` below is
/// what pins the `mutations_sync` setting: without it the mutation could still
/// be in flight and the row still resolvable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_delete_removes_an_unreferenced_type() {
    let Some(h) = common::bring_up_or_skip().await else {
        return;
    };
    let store = common::catalog_store(&h);

    store
        .create(common::fixture_usage_type(VCPU_GTS, "counter", &["region"]))
        .await
        .expect("create");

    store
        .delete(common::fixture_gts_id(VCPU_GTS))
        .await
        .expect("an unreferenced type must delete cleanly");

    let err = store
        .get(common::fixture_gts_id(VCPU_GTS))
        .await
        .expect_err("the type must be gone once delete returns");
    assert!(
        matches!(err, UsageCollectorPluginError::UsageTypeNotFound { .. }),
        "expected UsageTypeNotFound after delete, got {err:?}"
    );
}

/// Deleting a type that was never created is `UsageTypeNotFound`, not a silent
/// success: the gateway has to distinguish "already gone" from "deleted now".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_delete_of_an_absent_type_is_not_found() {
    let Some(h) = common::bring_up_or_skip().await else {
        return;
    };
    let store = common::catalog_store(&h);

    let err = store
        .delete(common::fixture_gts_id(MISSING_GTS))
        .await
        .expect_err("an absent type must not delete silently");
    assert!(
        matches!(err, UsageCollectorPluginError::UsageTypeNotFound { .. }),
        "expected UsageTypeNotFound, got {err:?}"
    );
}

/// A type with a usage record referencing it is refused, and the catalog row
/// survives the refusal.
///
/// This is the FK-emulation seam: `ClickHouse` has no `ON DELETE RESTRICT`, so
/// the pre-delete probe is the only thing standing between a delete and an
/// orphaned record. `sample_ref_count` must be a usable diagnostic (>= 1),
/// which is what the gear lifts into the 409 problem document.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_delete_of_a_referenced_type_is_refused() {
    let Some(h) = common::bring_up_or_skip().await else {
        return;
    };
    let store = common::catalog_store(&h);
    // The synchronous record store: an `async_insert` write can still be
    // buffered server-side when the probe runs, which would make this test
    // flaky for a reason that has nothing to do with what it asserts.
    let records = common::record_store_sync(&h);

    store
        .create(common::fixture_usage_type(RAM_GTS, "counter", &[]))
        .await
        .expect("create the type");
    records
        .create(common::fixture_usage_record(
            RAM_GTS,
            Uuid::new_v4(),
            "ref-guard-1",
            Decimal::from(1),
        ))
        .await
        .expect("create a referencing record");

    let err = store
        .delete(common::fixture_gts_id(RAM_GTS))
        .await
        .expect_err("a referenced type must not be deletable");
    match err {
        UsageCollectorPluginError::UsageTypeReferenced {
            gts_id,
            sample_ref_count,
        } => {
            assert_eq!(gts_id, common::fixture_gts_id(RAM_GTS));
            assert!(
                sample_ref_count >= 1,
                "sample_ref_count must be a usable diagnostic, got {sample_ref_count}"
            );
        }
        other => panic!("expected UsageTypeReferenced, got {other:?}"),
    }

    store
        .get(common::fixture_gts_id(RAM_GTS))
        .await
        .expect("a refused delete must leave the catalog row untouched");
}

/// Once the catalog row is gone, the record store refuses new records for that
/// `gts_id` on its own.
///
/// This is what bounds the accepted race window: the delete removes the
/// catalog row *before* sweeping, so from that moment the insert-time
/// existence check — not the sweep — is what keeps new orphans out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_records_are_refused_for_a_deleted_type() {
    let Some(h) = common::bring_up_or_skip().await else {
        return;
    };
    let store = common::catalog_store(&h);
    let records = common::record_store_sync(&h);

    store
        .create(common::fixture_usage_type(DISK_GTS, "counter", &[]))
        .await
        .expect("create the type");
    store
        .delete(common::fixture_gts_id(DISK_GTS))
        .await
        .expect("delete the unreferenced type");

    let err = records
        .create(common::fixture_usage_record(
            DISK_GTS,
            Uuid::new_v4(),
            "post-delete-1",
            Decimal::from(1),
        ))
        .await
        .expect_err("a record for a deleted type must be refused");
    assert!(
        matches!(err, UsageCollectorPluginError::UsageTypeNotFound { .. }),
        "expected UsageTypeNotFound, got {err:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_list_types_paginates_by_gts_id() {
    let Some(h) = common::bring_up_or_skip().await else {
        return;
    };
    let store = common::catalog_store(&h);

    for gts in [RAM_GTS, VCPU_GTS, DISK_GTS] {
        store
            .create(common::fixture_usage_type(gts, "counter", &[]))
            .await
            .expect("create usage type");
    }

    let mut expected = [DISK_GTS, RAM_GTS, VCPU_GTS];
    expected.sort_unstable();

    let page1 = store
        .list(&ODataQuery::new().with_limit(2))
        .await
        .expect("list first page");
    assert_eq!(page1.items.len(), 2, "first page capped at limit");
    assert_eq!(
        page1.items[0].gts_id.as_ref(),
        expected[0],
        "first item is the lexicographically-smallest gts_id"
    );
    assert_eq!(
        page1.items[1].gts_id.as_ref(),
        expected[1],
        "second item is the next gts_id"
    );
    let token = page1
        .page_info
        .next_cursor
        .expect("three types over limit 2 yield a next cursor");

    let cursor = CursorV1::decode(&token).expect("decode next cursor");
    let page2 = store
        .list(&ODataQuery::new().with_limit(2).with_cursor(cursor))
        .await
        .expect("list second page");
    assert_eq!(page2.items.len(), 1, "second page has the remaining type");
    assert_eq!(
        page2.items[0].gts_id.as_ref(),
        expected[2],
        "second page continues in gts_id order with no overlap"
    );
    assert!(
        page2.page_info.next_cursor.is_none(),
        "the final page has no next cursor"
    );
}

// ── `$filter` and cursor validation ──────────────────────────────────────────
//
// The catalog `$filter` allowlist admits only `gts_id` and `kind`; the cursor
// guards reject a continuation that does not belong to the query it is replayed
// against.

/// `list` with a `kind eq 'counter'` filter returns only counters.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_list_filters_by_kind() {
    let Some(h) = common::bring_up_or_skip().await else {
        return;
    };
    let store = common::catalog_store(&h);

    store
        .create(common::fixture_usage_type(RAM_GTS, "counter", &[]))
        .await
        .expect("create counter type");
    store
        .create(common::fixture_usage_type(VCPU_GTS, "gauge", &[]))
        .await
        .expect("create gauge type");

    let query = ODataQuery::new().with_filter(Expr::Compare(
        Box::new(Expr::Identifier("kind".to_owned())),
        CompareOperator::Eq,
        Box::new(Expr::Value(Value::String("counter".to_owned()))),
    ));

    let page = store.list(&query).await.expect("list filtered by kind");

    assert_eq!(page.items.len(), 1, "only the counter type matches");
    assert_eq!(page.items[0].gts_id.as_ref(), RAM_GTS);
    assert_eq!(page.items[0].kind, UsageKind::Counter);
}

/// A field outside the catalog allowlist cannot reach the SQL string: the filter
/// is rejected rather than silently ignored, which would return every row for a
/// query the caller believes is narrowed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_list_with_non_catalog_filter_field_is_internal() {
    let Some(h) = common::bring_up_or_skip().await else {
        return;
    };
    let store = common::catalog_store(&h);

    store
        .create(common::fixture_usage_type(RAM_GTS, "counter", &[]))
        .await
        .expect("create counter type");

    // `tenant_id` is a usage_records column, not a catalog one.
    let query = ODataQuery::new().with_filter(Expr::Compare(
        Box::new(Expr::Identifier("tenant_id".to_owned())),
        CompareOperator::Eq,
        Box::new(Expr::Value(Value::String("whatever".to_owned()))),
    ));

    let err = store
        .list(&query)
        .await
        .expect_err("a non-catalog filter field must be rejected");
    assert!(
        matches!(err, UsageCollectorPluginError::Internal(_)),
        "expected Internal, got {err:?}"
    );
}

/// A cursor minted for one `$filter` must not be replayed against another:
/// continuing with a mismatched filter would silently skip or repeat rows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_list_rejects_cursor_whose_filter_hash_disagrees() {
    let Some(h) = common::bring_up_or_skip().await else {
        return;
    };
    let store = common::catalog_store(&h);

    for gts in [RAM_GTS, VCPU_GTS, DISK_GTS] {
        store
            .create(common::fixture_usage_type(gts, "counter", &[]))
            .await
            .expect("create usage type");
    }

    // Page 1 carries no filter hash, so the cursor records `f: None`.
    let token = store
        .list(&ODataQuery::new().with_limit(2))
        .await
        .expect("list first page")
        .page_info
        .next_cursor
        .expect("three types over limit 2 yield a next cursor");
    let cursor = CursorV1::decode(&token).expect("decode next cursor");

    let err = store
        .list(
            &ODataQuery::new()
                .with_limit(2)
                .with_cursor(cursor)
                .with_filter_hash("a-different-filter".to_owned()),
        )
        .await
        .expect_err("a cursor from a differently-filtered query must be rejected");
    match err {
        UsageCollectorPluginError::Internal(msg) => assert!(
            msg.contains("cursor filter hash mismatch"),
            "unexpected message: {msg}"
        ),
        other => panic!("expected Internal, got {other:?}"),
    }
}

/// Backward paging is not implemented in v1, so a `d: "bwd"` cursor is rejected
/// rather than silently served as a forward page.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_list_rejects_backward_cursor() {
    let Some(h) = common::bring_up_or_skip().await else {
        return;
    };
    let store = common::catalog_store(&h);

    for gts in [RAM_GTS, VCPU_GTS, DISK_GTS] {
        store
            .create(common::fixture_usage_type(gts, "counter", &[]))
            .await
            .expect("create usage type");
    }

    let token = store
        .list(&ODataQuery::new().with_limit(2))
        .await
        .expect("list first page")
        .page_info
        .next_cursor
        .expect("three types over limit 2 yield a next cursor");
    let mut cursor = CursorV1::decode(&token).expect("decode next cursor");
    cursor.d = "bwd".to_owned();

    let err = store
        .list(&ODataQuery::new().with_limit(2).with_cursor(cursor))
        .await
        .expect_err("a backward cursor must be rejected");
    assert!(
        matches!(err, UsageCollectorPluginError::Internal(_)),
        "expected Internal, got {err:?}"
    );
}

// ── Backend-error classification ─────────────────────────────────────────────

/// Every catalog read/write surfaces a backend failure as a plugin error — none
/// swallows it or reports a false empty result.
///
/// `delete` is included: its first step is an existence read, so an
/// unreachable backend must surface as an error rather than as a
/// `UsageTypeNotFound` inferred from a read that never happened — the failure
/// mode that would silently report "already gone" for a type that is very much
/// still there.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_catalog_backend_failure_is_surfaced_by_every_operation() {
    let Some(h) = common::bring_up_or_skip().await else {
        return;
    };
    let store = common::catalog_store_over(&h, common::unreachable_client());

    store
        .get(common::fixture_gts_id(RAM_GTS))
        .await
        .expect_err("get must surface the backend failure");
    store
        .list(&ODataQuery::new())
        .await
        .expect_err("list must surface the backend failure");
    store
        .create(common::fixture_usage_type(RAM_GTS, "counter", &[]))
        .await
        .expect_err("create must surface the backend failure");

    let err = store
        .delete(common::fixture_gts_id(RAM_GTS))
        .await
        .expect_err("delete must surface the backend failure");
    assert!(
        !matches!(err, UsageCollectorPluginError::UsageTypeNotFound { .. }),
        "an unreachable backend must not be reported as an absent type, got {err:?}"
    );
}

/// `ChCatalogStore` holds a `clickhouse::Client` carrying the DSN, and therefore
/// the credentials. Its `Debug` must not print them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_catalog_store_debug_does_not_leak_the_dsn() {
    let Some(h) = common::bring_up_or_skip().await else {
        return;
    };
    let store = common::catalog_store(&h);

    let rendered = format!("{store:?}");
    assert!(
        rendered.starts_with("ChCatalogStore"),
        "unexpected Debug output: {rendered}"
    );
    assert!(
        !rendered.contains(common::CH_TEST_PASSWORD) && !rendered.contains("127.0.0.1"),
        "Debug must not expose the connection string: {rendered}"
    );
}

// ---------------------------------------------------------------------------
// Catalog writes stay synchronous
// ---------------------------------------------------------------------------

/// `usage_type_catalog` writes must **not** carry `async_insert`, independent
/// of the record store's `async_insert` config.
///
/// This is a control-plane table: writes come only from `create_usage_type`,
/// types are never deleted, and it is unpartitioned and tiny — so there is no
/// concurrent insert stream for the server-side buffer to coalesce and no part
/// count to reduce. Enabling it would only add the buffer-flush wait to a
/// request whose latency is directly observed.
///
/// Pinned as a test so a later "make the two insert sites consistent" refactor
/// has to argue with it rather than silently flip the behaviour.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_usage_type_catalog_insert_is_synchronous() {
    let Some(h) = common::bring_up_or_skip().await else {
        return;
    };
    let store = common::catalog_store(&h);

    store
        .create(common::fixture_usage_type(DISK_GTS, "counter", &[]))
        .await
        .expect("create usage type");

    // Matched by containment of the bare table name: the `clickhouse` crate
    // backtick-quotes the identifier (``INSERT INTO `usage_type_catalog`(…)``),
    // so a prefix match on the unquoted name finds nothing. `query_kind` already
    // restricts to inserts, and `usage_records` is not a substring of this name.
    let sql = "SELECT Settings['async_insert'] FROM system.query_log \
               WHERE type = 'QueryFinish' AND query_kind = 'Insert' \
                 AND positionCaseInsensitive(query, 'usage_type_catalog') > 0 \
               ORDER BY event_time_microseconds DESC LIMIT 1";

    let mut observed = None;
    for _ in 0..20 {
        h.client
            .query("SYSTEM FLUSH LOGS")
            .execute()
            .await
            .expect("flushing system logs must succeed");
        if let Ok(Some(setting)) = h.client.query(sql).fetch_optional::<String>().await {
            observed = Some(setting);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let observed =
        observed.expect("an INSERT into usage_type_catalog must appear in system.query_log");
    assert!(
        observed.is_empty() || observed == "0",
        "the catalog insert must not set async_insert (got {observed:?})"
    );
}
