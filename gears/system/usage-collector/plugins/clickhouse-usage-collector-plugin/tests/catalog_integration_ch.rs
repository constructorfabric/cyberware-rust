#![cfg(feature = "clickhouse")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
//! `ClickHouse`-backed integration tests for [`ChCatalogStore`]
//! (create / get / list / delete) plus the full coordination-lock test suite.
//!
//! # Standard catalog tests
//!
//! Mirror the `TimescaleDB` reference plugin catalog tests, adapted to
//! `ClickHouse`'s real-row-removal delete semantics (a lightweight `DELETE
//! FROM` under `ReplacingMergeTree(version)` + `FINAL`).
//!
//! # Coordination-lock tests
//!
//! These tests are `ClickHouse`-specific (no reference-plugin equivalent).
//! They prove that the per-`gts_id` exclusive lock (backed by
//! the cluster gear `DistributedLockV1`) closes the concurrent-reference race window with
//! zero residual (DESIGN.md §3.6, PRD.md §5).
//!
//! - `ch_concurrent_create_blocks_delete_and_delete_sees_reference` — a held
//!   exclusive create lock blocks a concurrent exclusive delete; after
//!   the create lock is released, the delete observes the reference and returns
//!   `UsageTypeReferenced`.
//! - `ch_concurrent_delete_removes_row_before_create_create_sees_not_found` —
//!   an exclusive delete lock blocks a concurrent exclusive create; once
//!   the row is deleted and the exclusive lock released, the create
//!   returns `UsageTypeNotFound`.
//! - `ch_lock_manager_fails_closed_when_profile_unbound` — unbound cluster profile fails closed.
//! - `ch_insert_against_deleted_type_is_not_found` — `create_usage_record`
//!   against a deleted `gts_id` returns `UsageTypeNotFound`.
//!
//! Requires Docker for `ClickHouse`. Cluster locks are in-process (no cluster).

mod common;

use std::sync::Arc;
use std::time::Duration;

use rust_decimal::Decimal;
use uuid::Uuid;

use toolkit_odata::ast::{CompareOperator, Expr, Value};
use toolkit_odata::{CursorV1, ODataQuery};

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_delete_unreferenced_succeeds() {
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
        .expect("delete unreferenced");

    // No settling delay: the DELETE carries `lightweight_deletes_sync = 2`, so
    // the row is gone by the time it returns.
    let err = store
        .get(common::fixture_gts_id(VCPU_GTS))
        .await
        .expect_err("get after delete must fail");
    assert!(
        matches!(err, UsageCollectorPluginError::UsageTypeNotFound { .. }),
        "get after delete must be UsageTypeNotFound, got {err:?}"
    );
}

/// Delete-then-recreate: `create` after `delete` must insert a fresh row, not
/// collide with the removed one.
///
/// `create` decides absorb vs `UsageTypeAlreadyExists` from a `FINAL`
/// pre-existence check, so a delete whose removal is not yet visible turns a
/// legitimate re-registration into a conflict. The differing payload is
/// deliberate: an identical one would be absorbed, which cannot distinguish "the
/// row was recreated" from "the old row is still there".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_delete_then_recreate_same_gts_id_succeeds() {
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
        .expect("delete unreferenced");

    let recreated = common::fixture_usage_type(VCPU_GTS, "gauge", &["tier"]);
    let created = store
        .create(recreated.clone())
        .await
        .expect("re-create after delete must not conflict with the removed row");
    assert_eq!(created, recreated, "re-create returns the new payload");

    let fetched = store
        .get(common::fixture_gts_id(VCPU_GTS))
        .await
        .expect("get after re-create");
    assert_eq!(
        fetched, recreated,
        "the stored row is the re-created one, not the deleted original"
    );
}

/// The `DELETE` must be synchronous even when the connection asks for async.
///
/// This is the test that actually pins `with_setting("lightweight_deletes_sync",
/// "2")` in `delete_under_lock`. Every other delete-visibility assertion in this
/// file passes with or without it, because the test container's own default is
/// already `2` — so they prove the server's behaviour, not the plugin's. Here the
/// client asks for `0` (return before the removal is visible), which the
/// statement-level setting must override.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_delete_is_synchronous_even_when_the_connection_default_is_async() {
    let Some(h) = common::bring_up_or_skip().await else {
        return;
    };
    let async_delete_client = h
        .client
        .clone()
        .with_setting("lightweight_deletes_sync", "0");
    let store = common::catalog_store_over(&h, async_delete_client);

    store
        .create(common::fixture_usage_type(DISK_GTS, "counter", &[]))
        .await
        .expect("create");
    store
        .delete(common::fixture_gts_id(DISK_GTS))
        .await
        .expect("delete unreferenced");

    let err = store
        .get(common::fixture_gts_id(DISK_GTS))
        .await
        .expect_err("the statement-level setting must override the connection default");
    assert!(
        matches!(err, UsageCollectorPluginError::UsageTypeNotFound { .. }),
        "get immediately after delete must be UsageTypeNotFound, got {err:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_delete_missing_is_not_found() {
    let Some(h) = common::bring_up_or_skip().await else {
        return;
    };
    let store = common::catalog_store(&h);

    let err = store
        .delete(common::fixture_gts_id(MISSING_GTS))
        .await
        .expect_err("delete absent must fail");
    assert!(
        matches!(err, UsageCollectorPluginError::UsageTypeNotFound { .. }),
        "delete absent must be UsageTypeNotFound, got {err:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_delete_referenced_is_usage_type_referenced() {
    let Some(h) = common::bring_up_or_skip().await else {
        return;
    };
    let store = common::catalog_store(&h);
    let record_store = common::record_store(&h);

    store
        .create(common::fixture_usage_type(RAM_GTS, "counter", &[]))
        .await
        .expect("create");

    // Insert a usage record to create a reference.
    record_store
        .create(common::fixture_usage_record(
            RAM_GTS,
            Uuid::from_u128(0xABCD),
            "idem-ref",
            Decimal::ONE,
        ))
        .await
        .expect("insert usage record to create reference");

    let err = store
        .delete(common::fixture_gts_id(RAM_GTS))
        .await
        .expect_err("delete referenced must fail");
    match err {
        UsageCollectorPluginError::UsageTypeReferenced {
            sample_ref_count, ..
        } => assert!(
            sample_ref_count >= 1,
            "sample_ref_count must be >= 1, got {sample_ref_count}"
        ),
        other => panic!("delete referenced must be UsageTypeReferenced, got {other:?}"),
    }
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

// ── Referential-integrity lock tests ─────────────────────────────────────────
//
// These tests prove the `gts_id` coordination lock closes the concurrent-
// reference race window with zero residual (DESIGN.md §3.6, PRD.md §5
// `cpt-cf-uc-ch-plugin-fr-referential-integrity`).

/// A held exclusive create lock blocks a concurrent exclusive delete;
/// once the create lock is released, the delete returns `UsageTypeReferenced`
/// because a usage record was already inserted.
///
/// Proof of locking: the delete task does not complete while the create lock is
/// held (verified by `tokio::time::timeout`). After release the delete's own
/// acquisition of the same mutex succeeds and the reference probe returns a
/// non-zero count.
///
/// **Hard assertion**: the delete NEVER observes zero references while the
/// create lock is in flight.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_concurrent_create_blocks_delete_and_delete_sees_reference() {
    let Some(h) = common::bring_up_or_skip().await else {
        return;
    };

    let catalog_store = common::catalog_store(&h);
    let record_store = common::record_store(&h);

    // 1. Register VCPU_GTS.
    catalog_store
        .create(common::fixture_usage_type(VCPU_GTS, "counter", &[]))
        .await
        .expect("register usage type");

    // 2. Insert a usage record to create a reference (this also acquires +
    //    releases the gts_id mutex internally).
    record_store
        .create(common::fixture_usage_record(
            VCPU_GTS,
            Uuid::from_u128(0xCC01),
            "idem-lock-cc1",
            Decimal::ONE,
        ))
        .await
        .expect("insert usage record to create reference");

    // 3. Acquire the create lock directly, simulating a create call in flight.
    //    The cluster lock is an exclusive per-gts_id mutex; a lock acquired by
    //    this separate LockManager session will block any concurrent acquisition
    //    by the catalog_store's own LockManager.
    let lm = common::lock_manager(&h.hub);
    let shared_guard = lm
        .acquire_for_create(VCPU_GTS)
        .await
        .expect("acquire create lock for simulation");

    // 4. Spawn the delete task. It tries to acquire the same per-gts_id mutex for
    //    VCPU_GTS but is blocked by our held create lock.
    let cs_del = catalog_store.clone();
    let delete_task =
        tokio::spawn(async move { cs_del.delete(common::fixture_gts_id(VCPU_GTS)).await });

    // 5. The delete must NOT complete while the create lock is held. Give it
    //    250 ms of head start; if it finished in that window it opened the race.
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        !delete_task.is_finished(),
        "delete MUST be blocked by the held create lock; if it completed, the lock did not block it"
    );

    // 6. Release the exclusive create lock. The delete task's lock acquisition
    //    now proceeds.
    shared_guard
        .release()
        .await
        .expect("release create lock for simulation");

    // 7. Wait for the delete task to complete; it must return UsageTypeReferenced
    //    because the usage record inserted in step 2 is still present.
    let delete_result = tokio::time::timeout(Duration::from_secs(5), delete_task)
        .await
        .expect("delete task must complete after the create lock is released")
        .expect("delete task must not panic");

    match delete_result {
        Err(UsageCollectorPluginError::UsageTypeReferenced {
            sample_ref_count, ..
        }) => {
            assert!(
                sample_ref_count >= 1,
                "sample_ref_count must be >= 1; the inserted record is a reference"
            );
        }
        other => {
            panic!("expected UsageTypeReferenced after releasing the create lock, got {other:?}")
        }
    }
}

/// An exclusive delete lock held by a delete call blocks a concurrent create,
/// which contends for the same per-`gts_id` mutex; once the row is deleted and
/// the lock is released, the create observes the deletion and returns
/// `UsageTypeNotFound`.
/// No orphan record is persisted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_concurrent_delete_removes_row_before_create_create_sees_not_found() {
    let Some(h) = common::bring_up_or_skip().await else {
        return;
    };

    let catalog_store = common::catalog_store(&h);
    let record_store = common::record_store(&h);

    // 1. Register VCPU_GTS.
    catalog_store
        .create(common::fixture_usage_type(VCPU_GTS, "counter", &[]))
        .await
        .expect("register usage type");

    // 2. Delete VCPU_GTS (no references -> row removed immediately).
    catalog_store
        .delete(common::fixture_gts_id(VCPU_GTS))
        .await
        .expect("delete unreferenced usage type (removes the row)");

    // 3. Acquire an exclusive lock directly, simulating a second delete in flight
    //    (or just the deletion already committed state). The record_store's
    //    create will try to acquire the same per-gts_id mutex, which MUST wait
    //    for this exclusive lock to be released.
    let lm = common::lock_manager(&h.hub);
    let exclusive_guard = lm
        .acquire_for_delete(VCPU_GTS)
        .await
        .expect("acquire exclusive lock for simulation");

    // 4. Spawn create task; it will block at lock acquisition because the
    //    exclusive lock is held.
    let rs_create = record_store.clone();
    let create_task = tokio::spawn(async move {
        rs_create
            .create(common::fixture_usage_record(
                VCPU_GTS,
                Uuid::from_u128(0xCD01),
                "idem-del-before-create",
                Decimal::ONE,
            ))
            .await
    });

    // 5. Assert the create task is blocked while the exclusive lock is held.
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        !create_task.is_finished(),
        "create MUST be blocked by the held exclusive lock"
    );

    // 6. Release the exclusive lock. The create task's lock acquisition
    //    now proceeds; it checks the catalog and finds the row gone.
    exclusive_guard
        .release()
        .await
        .expect("release delete lock for simulation");

    // 7. Create task must return UsageTypeNotFound (deletion visible via FINAL).
    let create_result = tokio::time::timeout(Duration::from_secs(5), create_task)
        .await
        .expect("create task must complete after exclusive lock is released")
        .expect("create task must not panic");

    match create_result {
        Err(UsageCollectorPluginError::UsageTypeNotFound { gts_id }) => {
            assert_eq!(
                gts_id,
                common::fixture_gts_id(VCPU_GTS),
                "UsageTypeNotFound carries the deleted gts_id"
            );
        }
        other => panic!("expected UsageTypeNotFound after the row was deleted, got {other:?}"),
    }
}

/// Regression test for the `create_batch` single-`gts_id` assumption
/// (DESIGN.md §3.6 Batch Ingest): a batch's SECOND record's `gts_id` must be
/// covered by its own coordination lock, not just `records[0]`'s. Before the
/// per-`gts_id` partitioning fix, `create_batch` locked and validated only
/// `records[0].gts_id`, so a concurrent `delete_usage_type` targeting a
/// later record's `gts_id` would never be blocked and could delete the
/// type out from under an in-flight insert, orphaning the record.
///
/// Proof of locking: the create lock held directly on `RAM_GTS` (simulating
/// `create_batch`'s second-partition lock being in flight) blocks a
/// concurrent `delete_usage_type(RAM_GTS)` (verified via `is_finished`).
/// Once released, the delete succeeds (no references yet); a subsequent
/// batch whose SECOND record targets the now-deleted `RAM_GTS` then
/// correctly rejects only that record — no orphan is ever persisted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_batch_second_record_gts_id_lock_blocks_delete_of_that_type() {
    let Some(h) = common::bring_up_or_skip().await else {
        return;
    };

    let catalog_store = common::catalog_store(&h);
    let record_store = common::record_store(&h);

    catalog_store
        .create(common::fixture_usage_type(VCPU_GTS, "counter", &[]))
        .await
        .expect("register type A (VCPU_GTS)");
    catalog_store
        .create(common::fixture_usage_type(RAM_GTS, "counter", &[]))
        .await
        .expect("register type B (RAM_GTS)");

    // 1. Acquire the create lock directly on RAM_GTS, simulating a `create_batch`
    //    call whose SECOND record's partition already holds RAM_GTS's per-gts_id
    //    mutex and is mid-flight.
    let lm = common::lock_manager(&h.hub);
    let shared_guard = lm
        .acquire_for_create(RAM_GTS)
        .await
        .expect("acquire create lock for simulation");

    // 2. Spawn a delete of RAM_GTS. It must block on the same mutex while
    //    the simulated batch partition's create lock is held.
    let cs_del = catalog_store.clone();
    let delete_task =
        tokio::spawn(async move { cs_del.delete(common::fixture_gts_id(RAM_GTS)).await });

    // 3. Assert the delete is blocked while the create lock is held. Before
    //    the fix, `create_batch` never took a lock for a non-first record's
    //    `gts_id`, so this delete would never have been blocked by it.
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        !delete_task.is_finished(),
        "delete of the batch's SECOND gts_id must be blocked by its own create lock"
    );

    // 4. Release the create lock; the delete's own acquisition of the same
    //    mutex proceeds. RAM_GTS has no references yet, so it succeeds.
    shared_guard
        .release()
        .await
        .expect("release create lock for simulation");
    let delete_result = tokio::time::timeout(Duration::from_secs(5), delete_task)
        .await
        .expect("delete task must complete after the create lock is released")
        .expect("delete task must not panic");
    assert!(
        delete_result.is_ok(),
        "RAM_GTS has no references yet; delete must succeed once unblocked: {delete_result:?}"
    );

    // 5. A batch whose SECOND record targets the now-deleted RAM_GTS must
    //    reject only that record — the first (VCPU_GTS) record is untouched.
    let tenant = Uuid::from_u128(0xB2B2);
    let row_a = common::fixture_usage_record(VCPU_GTS, tenant, "batch-lock-a", Decimal::ONE);
    let row_b = common::fixture_usage_record(RAM_GTS, tenant, "batch-lock-b", Decimal::ONE);

    let results = record_store
        .create_batch(vec![row_a.clone(), row_b.clone()])
        .await
        .expect("batch call itself must not fail outright");

    assert_eq!(results.len(), 2, "one outcome per input record, in order");
    let r0 = results[0]
        .as_ref()
        .expect("row 0 (VCPU_GTS) is unaffected by RAM_GTS's deletion");
    assert_eq!(r0.id, row_a.id, "row 0 preserves position");

    match results[1].as_ref() {
        Err(UsageCollectorPluginError::UsageTypeNotFound { gts_id }) => {
            assert_eq!(
                *gts_id,
                common::fixture_gts_id(RAM_GTS),
                "row 1 fails its own catalog check against the deleted RAM_GTS"
            );
        }
        other => panic!(
            "row referencing the deleted RAM_GTS must be rejected, no orphan may be \
             persisted, got {other:?}"
        ),
    }

    let err = record_store
        .get(row_b.id)
        .await
        .expect_err("row referencing a deleted gts_id must never be written (no orphan)");
    assert!(
        matches!(err, UsageCollectorPluginError::UsageRecordNotFound { .. }),
        "expected UsageRecordNotFound, got {err:?}"
    );
}

/// Acquire fails closed when the `usage-collector` cluster profile has no
/// lock backend registered.
///
/// Needs no container: the empty [`ClientHub`](toolkit::client_hub::ClientHub)
/// makes the profile unresolvable, so `acquire_for_create` fails before any
/// I/O.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ch_lock_manager_fails_closed_when_profile_unbound() {
    let hub = std::sync::Arc::new(toolkit::client_hub::ClientHub::default());
    let mgr = common::lock_manager_with_timeout(&hub, std::time::Duration::from_millis(200));
    let result = mgr
        .acquire_for_create("gts.cf.toolkit.plugins.plugin.v1~cf.core.uc.type.v1~unbound")
        .await;
    assert!(result.is_err(), "unbound profile must fail closed");
    let err = result.err().unwrap();
    assert!(
        matches!(
            err,
            usage_collector_sdk::UsageCollectorPluginError::Transient { .. }
        ),
        "expected Transient, got {err:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_insert_against_deleted_type_is_not_found() {
    let Some(h) = common::bring_up_or_skip().await else {
        return;
    };

    let catalog_store = common::catalog_store(&h);
    let record_store = common::record_store(&h);

    // Register then delete VCPU_GTS (unreferenced -> row removed).
    catalog_store
        .create(common::fixture_usage_type(VCPU_GTS, "counter", &[]))
        .await
        .expect("register usage type");
    catalog_store
        .delete(common::fixture_gts_id(VCPU_GTS))
        .await
        .expect("delete unreferenced (removes the row)");

    // Attempt to create a usage record against the now-deleted type. No
    // settling delay: the DELETE waits for the removal to be visible.
    let err = record_store
        .create(common::fixture_usage_record(
            VCPU_GTS,
            Uuid::from_u128(0xEB01),
            "idem-deleted-type",
            Decimal::ONE,
        ))
        .await
        .expect_err("insert against deleted type must fail");

    match err {
        UsageCollectorPluginError::UsageTypeNotFound { gts_id } => {
            assert_eq!(
                gts_id,
                common::fixture_gts_id(VCPU_GTS),
                "UsageTypeNotFound carries the deleted gts_id"
            );
        }
        other => panic!("expected UsageTypeNotFound for deleted type, got {other:?}"),
    }
}

// ── `$filter` and cursor validation ──────────────────────────────────────────
//
// The catalog `$filter` allowlist admits only `gts_id` and `kind`; the cursor
// guards reject a continuation that does not belong to the query it is replayed
// against. Both were previously unexercised.

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
/// The store keeps the harness's live lock backend (so `delete` reaches its SQL
/// instead of failing at lock acquisition) but points at a port with nothing
/// listening.
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
    store
        .delete(common::fixture_gts_id(RAM_GTS))
        .await
        .expect_err("delete must surface the backend failure");
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

// ── Lock-guard failure paths against a live client ───────────────────────────
//
// These reach the guard calls inside `delete`'s critical section, which an
// offline client cannot: the existence check and reference probe run first and
// would fail on SQL before the guard is ever consulted.

/// Losing the lease between the reference probe and the `DELETE` must abort the
/// delete: without the lock the probe is no longer authoritative, so removing
/// the row could orphan a concurrently-inserted usage record.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_delete_aborts_when_lease_is_lost_before_the_write() {
    let Some(h) = common::bring_up_or_skip().await else {
        return;
    };

    common::catalog_store(&h)
        .create(common::fixture_usage_type(RAM_GTS, "counter", &[]))
        .await
        .expect("seed usage type");

    let store = common::catalog_store_with_lock(&h, Arc::new(stubs::LeaseLostLock));

    let err = store
        .delete(common::fixture_gts_id(RAM_GTS))
        .await
        .expect_err("a lost lease must abort the delete");
    assert!(
        matches!(err, UsageCollectorPluginError::Transient { .. }),
        "a lost lease is retryable, got {err:?}"
    );

    // The row survives: the delete was aborted, not merely reported as failed.
    common::catalog_store(&h)
        .get(common::fixture_gts_id(RAM_GTS))
        .await
        .expect("the usage type must still exist after the aborted delete");
}

/// A release failure after a successful `DELETE` is surfaced to the caller: the
/// lock may stay held cluster-side until its TTL lapses, so the caller must not
/// treat the operation as fully complete.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_delete_surfaces_release_failure_after_removing_the_row() {
    let Some(h) = common::bring_up_or_skip().await else {
        return;
    };

    common::catalog_store(&h)
        .create(common::fixture_usage_type(DISK_GTS, "counter", &[]))
        .await
        .expect("seed usage type");

    let store = common::catalog_store_with_lock(&h, Arc::new(stubs::ReleaseFailsLock));

    let err = store
        .delete(common::fixture_gts_id(DISK_GTS))
        .await
        .expect_err("a release failure must be surfaced");
    assert!(
        matches!(err, UsageCollectorPluginError::Transient { .. }),
        "expected the release error, got {err:?}"
    );

    // The row is gone: the DELETE ran before the release failed.
    let err = common::catalog_store(&h)
        .get(common::fixture_gts_id(DISK_GTS))
        .await
        .expect_err("the row was deleted before the release failed");
    assert!(
        matches!(err, UsageCollectorPluginError::UsageTypeNotFound { .. }),
        "expected UsageTypeNotFound, got {err:?}"
    );
}

/// Lock stubs that grant the lock but fail on one specific guard call, so the
/// failure lands inside `delete`'s critical section.
mod stubs {
    use async_trait::async_trait;
    use usage_collector_sdk::UsageCollectorPluginError;

    use clickhouse_usage_collector_plugin::infra::coordination::lock_manager::LockGuardPort;
    use clickhouse_usage_collector_plugin::infra::storage::catalog_store::CatalogLockPort;

    /// Guard whose lease renew fails.
    pub struct LeaseLostGuard;

    #[async_trait]
    impl LockGuardPort for LeaseLostGuard {
        async fn ensure_still_held(&self) -> Result<(), UsageCollectorPluginError> {
            Err(UsageCollectorPluginError::transient(
                "cluster lock lease lost (test stub)",
            ))
        }

        async fn release(self: Box<Self>) -> Result<(), UsageCollectorPluginError> {
            Ok(())
        }
    }

    pub struct LeaseLostLock;

    #[async_trait]
    impl CatalogLockPort for LeaseLostLock {
        async fn acquire_exclusive_for_delete(
            &self,
            _gts_id: &str,
        ) -> Result<Box<dyn LockGuardPort>, UsageCollectorPluginError> {
            Ok(Box::new(LeaseLostGuard))
        }
    }

    /// Guard that holds the lease but cannot be released.
    pub struct ReleaseFailsGuard;

    #[async_trait]
    impl LockGuardPort for ReleaseFailsGuard {
        async fn ensure_still_held(&self) -> Result<(), UsageCollectorPluginError> {
            Ok(())
        }

        async fn release(self: Box<Self>) -> Result<(), UsageCollectorPluginError> {
            Err(UsageCollectorPluginError::transient(
                "cluster lock release failed (test stub)",
            ))
        }
    }

    pub struct ReleaseFailsLock;

    #[async_trait]
    impl CatalogLockPort for ReleaseFailsLock {
        async fn acquire_exclusive_for_delete(
            &self,
            _gts_id: &str,
        ) -> Result<Box<dyn LockGuardPort>, UsageCollectorPluginError> {
            Ok(Box::new(ReleaseFailsGuard))
        }
    }
}
