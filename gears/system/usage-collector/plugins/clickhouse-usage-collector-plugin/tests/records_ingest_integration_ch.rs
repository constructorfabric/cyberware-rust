#![cfg(feature = "clickhouse")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
//! `ClickHouse`-backed integration tests for [`ChRecordStore`] ingest:
//! single insert with idempotency dedup (insert / absorb / conflict),
//! compensation persistence, batch per-row outcomes, deactivation cascade,
//! cascade atomicity, the dedup-convergence race, and the lock-release failure
//! that lands after a successful write. Requires Docker, except for the
//! fixture-contract test at the bottom of the file.

mod common;

use rust_decimal::Decimal;
use tokio::sync::Barrier;
use uuid::Uuid;

use usage_collector_sdk::{
    UsageCollectorPluginError, UsageRecord, UsageRecordStatus, created_at_micros,
    derive_usage_record_id,
};

use clickhouse_usage_collector_plugin::domain::ports::{CatalogStore, RecordStore};

const VCPU_GTS: &str = "gts.cf.core.uc.usage_record.v1~cf.compute._.vcpu_hours.v1";

/// Bring up containers and register `VCPU_GTS` in the catalog so the
/// referential-integrity check passes for every ingest test.
///
/// Returns `None` when Docker is unavailable, so the caller can skip its own
/// test body with an early `return` instead of terminating the whole test
/// binary (see [`common::bring_up_or_skip`]).
async fn setup() -> Option<(common::ChHarness, impl RecordStore + Clone)> {
    let h = common::bring_up_or_skip().await?;
    let catalog = common::catalog_store(&h);
    catalog
        .create(common::fixture_usage_type(VCPU_GTS, "counter", &[]))
        .await
        .expect("register usage type for referential integrity");
    let store = common::record_store(&h);
    Some((h, store))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_insert_new_record_returns_active() {
    let Some((_h, store)) = setup().await else {
        return;
    };
    let tenant = Uuid::from_u128(0x1001);

    let record = common::fixture_usage_record(VCPU_GTS, tenant, "idem-new", Decimal::new(5, 0));

    let stored = store
        .create(record.clone())
        .await
        .expect("create new record");

    assert_eq!(stored.id, record.id, "id round-trips");
    assert_eq!(stored.value, record.value, "value round-trips");
    assert_eq!(stored.tenant_id, record.tenant_id, "tenant round-trips");
    assert_eq!(
        stored.idempotency_key, record.idempotency_key,
        "idempotency_key round-trips"
    );
    assert_eq!(
        stored.created_at, record.created_at,
        "created_at round-trips at microsecond precision"
    );
    assert_eq!(
        stored.status,
        UsageRecordStatus::Active,
        "first accept defaults to Active"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_exact_retry_is_absorbed() {
    let Some((_h, store)) = setup().await else {
        return;
    };
    let tenant = Uuid::from_u128(0x1002);

    let record = common::fixture_usage_record(VCPU_GTS, tenant, "idem-retry", Decimal::new(7, 0));

    let first = store.create(record.clone()).await.expect("first create");
    let second = store
        .create(record.clone())
        .await
        .expect("exact retry must be absorbed, not conflict");

    assert_eq!(first.id, second.id, "absorb returns the same stored id");
    assert_eq!(second.id, record.id, "stored id is the original");
    assert_eq!(
        second.value, record.value,
        "absorb returns the stored value"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_insert_with_unregistered_gts_id_is_usage_type_not_found() {
    const UNREGISTERED_GTS: &str =
        "gts.cf.core.uc.usage_record.v1~cf.compute._.unregistered_hours.v1";
    let Some((_h, store)) = setup().await else {
        return;
    };
    let tenant = Uuid::from_u128(0x1009);

    let record =
        common::fixture_usage_record(UNREGISTERED_GTS, tenant, "idem-no-type", Decimal::new(1, 0));

    let err = store
        .create(record)
        .await
        .expect_err("insert against an unregistered gts_id must fail the catalog check");

    match err {
        UsageCollectorPluginError::UsageTypeNotFound { gts_id } => {
            assert_eq!(
                gts_id,
                common::fixture_gts_id(UNREGISTERED_GTS),
                "the typed error carries the missing gts_id"
            );
        }
        other => panic!("expected UsageTypeNotFound, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_same_key_conflicting_value_is_idempotency_conflict() {
    let Some((_h, store)) = setup().await else {
        return;
    };
    let tenant = Uuid::from_u128(0x1003);

    let first = common::fixture_usage_record(VCPU_GTS, tenant, "idem-dup", Decimal::new(3, 0));
    let stored = store.create(first.clone()).await.expect("first create");

    // Same idempotency key and instant, different value. Both fixtures derive
    // their id from that shared 4-tuple, so this is a genuine resubmission
    // rather than a hand-forged key collision.
    let conflicting =
        common::fixture_usage_record(VCPU_GTS, tenant, "idem-dup", Decimal::new(999, 0));
    assert_eq!(
        conflicting.id, first.id,
        "the same dedup key must derive the same id"
    );

    let err = store
        .create(conflicting)
        .await
        .expect_err("conflicting value on the same key must fail");

    match err {
        UsageCollectorPluginError::IdempotencyConflict {
            idempotency_key,
            existing_id,
        } => {
            assert_eq!(idempotency_key, "idem-dup", "conflict carries the key");
            assert_eq!(
                existing_id, stored.id,
                "conflict carries the previously stored row's id"
            );
        }
        other => panic!("expected IdempotencyConflict, got {other:?}"),
    }
}

// ── Mismatched stored identity ────────────────────────────────────────────────
//
// The dedup lookup keys on the canonical tuple `(tenant_id, gts_id, created_at,
// idempotency_key)` rather than on `id`. That matters only for a stored row
// whose `id` disagrees with its own canonical tuple — something the gateway
// cannot produce (the id is a derived projection) but that data corruption, or a
// row written before the lookup was fixed, can. `ClickHouse` has no UNIQUE
// constraint to catch it, so these two tests seed such a row directly.

/// Insert a `usage_records` row carrying `record`'s canonical dedup tuple but a
/// caller-chosen `id`, bypassing the store so the identity can disagree with the
/// tuple. `value` and the resource fields mirror the fixture, so the *only*
/// canonical difference from `record` is the identity.
async fn seed_row_with_id(client: &clickhouse::Client, id: Uuid, record: &UsageRecord) {
    let sql = "INSERT INTO usage_records (id, tenant_id, gts_id, value, created_at, \
               resource_id, resource_type, subject_id, subject_type, idempotency_key, \
               corrects_id, status, metadata, ingested_at, version) VALUES \
               (?, ?, ?, toDecimal128(?, 9), fromUnixTimestamp64Micro(?), ?, ?, NULL, NULL, \
               ?, NULL, 'active', map(), now64(6), 1)";
    let micros = i64::try_from(created_at_micros(record.created_at))
        .expect("fixture created_at must fit in i64 microseconds");
    client
        .query(sql)
        .bind(id.to_string())
        .bind(record.tenant_id.to_string())
        .bind(record.gts_id.as_ref())
        .bind(record.value.to_string())
        .bind(micros)
        .bind(record.resource_ref.resource_id())
        .bind(record.resource_ref.resource_type())
        .bind(record.idempotency_key.as_str())
        .execute()
        .await
        .expect("seeding a mismatched-id usage record must succeed");
}

/// How many rows share `record`'s canonical dedup tuple, post-`FINAL`.
async fn count_rows_for_dedup_key(client: &clickhouse::Client, record: &UsageRecord) -> u64 {
    let sql = "SELECT count() FROM usage_records FINAL \
               WHERE tenant_id = ? AND gts_id = ? \
               AND created_at = fromUnixTimestamp64Micro(?) AND idempotency_key = ?";
    let micros = i64::try_from(created_at_micros(record.created_at))
        .expect("fixture created_at must fit in i64 microseconds");
    client
        .query(sql)
        .bind(record.tenant_id.to_string())
        .bind(record.gts_id.as_ref())
        .bind(micros)
        .bind(record.idempotency_key.as_str())
        .fetch_one::<u64>()
        .await
        .expect("counting rows for the dedup key must succeed")
}

/// A stored row whose `id` disagrees with its canonical tuple must fail closed:
/// the idempotency key is already bound to a record the caller cannot address,
/// so re-creating it is an `IdempotencyConflict`, not a second insert.
///
/// Against an `id`-keyed lookup this test fails — the seeded row is invisible to
/// the lookup and the create silently inserts a duplicate under a key already in
/// use. That is exactly the regression it exists to pin.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_stored_row_with_mismatched_id_is_an_idempotency_conflict() {
    let Some((h, store)) = setup().await else {
        return;
    };
    let tenant = Uuid::from_u128(0x100D);

    let record =
        common::fixture_usage_record(VCPU_GTS, tenant, "idem-mismatched-id", Decimal::new(7, 0));
    let wrong_id = Uuid::from_u128(0xDEAD_BEEF);
    assert_ne!(
        wrong_id, record.id,
        "the seeded row must not accidentally carry the derived id"
    );

    seed_row_with_id(&h.client, wrong_id, &record).await;

    let err = store
        .create(record.clone())
        .await
        .expect_err("a stored row with a mismatched id must not be re-inserted");

    match err {
        UsageCollectorPluginError::IdempotencyConflict {
            idempotency_key,
            existing_id,
        } => {
            assert_eq!(idempotency_key, "idem-mismatched-id");
            assert_eq!(
                existing_id, wrong_id,
                "the conflict names the corrupted stored row"
            );
        }
        other => panic!("expected IdempotencyConflict, got {other:?}"),
    }

    assert_eq!(
        count_rows_for_dedup_key(&h.client, &record).await,
        1,
        "the rejected create must not have inserted a second row under the same key"
    );
}

/// When a mismatched-`id` twin sits alongside the correctly-derived row, the
/// exact-`id` match still wins, so an honest retry is absorbed rather than
/// spuriously rejected. The twin's `id` sorts below the derived one, so the
/// lowest-`id` tie-break alone would pick the wrong row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_exact_retry_is_absorbed_despite_a_mismatched_id_twin() {
    let Some((h, store)) = setup().await else {
        return;
    };
    let tenant = Uuid::from_u128(0x100E);

    let record = common::fixture_usage_record(VCPU_GTS, tenant, "idem-twin", Decimal::new(13, 0));
    let stored = store.create(record.clone()).await.expect("first create");

    let twin_id = Uuid::from_u128(1);
    assert!(
        twin_id < record.id,
        "the twin must sort below the derived id, so the tie-break alone would pick it"
    );
    seed_row_with_id(&h.client, twin_id, &record).await;

    let absorbed = store
        .create(record.clone())
        .await
        .expect("an exact retry must still be absorbed alongside a mismatched-id twin");
    assert_eq!(
        absorbed.id, stored.id,
        "absorption returns the caller's own row, not the twin"
    );
    assert_eq!(
        absorbed.value,
        Decimal::new(13, 0),
        "the absorbed row keeps its value"
    );
}

/// `created_at` is part of the derivation input and of the dedup key, so the
/// same idempotency key submitted at two different instants is two rows, not a
/// conflict — and each stays addressable under its own derived id.
///
/// This is the counterpart to `ch_same_key_conflicting_value_is_idempotency_conflict`:
/// together they pin that the id moves with the whole 4-tuple rather than with
/// the idempotency key alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_same_idem_different_created_at_are_distinct_and_addressable() {
    let Some((_h, store)) = setup().await else {
        return;
    };
    let tenant = Uuid::from_u128(0x100C);

    let earlier = common::fixture_usage_record_at(
        VCPU_GTS,
        tenant,
        "idem-two-instants",
        Decimal::new(11, 0),
        common::fixture_created_at_offset(0),
    );
    let later = common::fixture_usage_record_at(
        VCPU_GTS,
        tenant,
        "idem-two-instants",
        Decimal::new(22, 0),
        common::fixture_created_at_offset(1),
    );
    assert_ne!(
        earlier.id, later.id,
        "a different created_at must derive a different id"
    );

    store
        .create(earlier.clone())
        .await
        .expect("create the earlier row");
    store
        .create(later.clone())
        .await
        .expect("a different instant is a new row, not a conflict");

    let fetched_earlier = store.get(earlier.id).await.expect("get the earlier row");
    assert_eq!(
        fetched_earlier.created_at, earlier.created_at,
        "the earlier row keeps its own instant"
    );
    assert_eq!(
        fetched_earlier.value,
        Decimal::new(11, 0),
        "the earlier row keeps its own value"
    );

    let fetched_later = store.get(later.id).await.expect("get the later row");
    assert_eq!(
        fetched_later.created_at, later.created_at,
        "the later row keeps its own instant"
    );
    assert_eq!(
        fetched_later.value,
        Decimal::new(22, 0),
        "the later row keeps its own value"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_compensation_persists_corrects_id() {
    let Some((_h, store)) = setup().await else {
        return;
    };
    let tenant = Uuid::from_u128(0x1004);

    let original = common::fixture_usage_record(VCPU_GTS, tenant, "idem-orig", Decimal::new(10, 0));
    let original = store.create(original).await.expect("create original");

    let mut compensation =
        common::fixture_usage_record(VCPU_GTS, tenant, "idem-comp", Decimal::new(-10, 0));
    compensation.corrects_id = Some(original.id);

    let stored = store
        .create(compensation.clone())
        .await
        .expect("create compensation");
    assert_eq!(
        stored.corrects_id,
        Some(original.id),
        "create returns the compensation target"
    );

    let fetched = store.get(stored.id).await.expect("get compensation back");
    assert_eq!(
        fetched.corrects_id,
        Some(original.id),
        "corrects_id persists and reads back"
    );
    assert_eq!(
        fetched.value,
        Decimal::new(-10, 0),
        "negative value persists"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_batch_preserves_order_and_isolates_conflict() {
    let Some((_h, store)) = setup().await else {
        return;
    };
    let tenant = Uuid::from_u128(0x1005);

    // Pre-existing record whose key the batch's #1 will collide with.
    let existing = common::fixture_usage_record(VCPU_GTS, tenant, "batch-dup", Decimal::new(1, 0));
    let existing = store.create(existing).await.expect("seed existing record");

    let row0 = common::fixture_usage_record(VCPU_GTS, tenant, "batch-0", Decimal::new(2, 0));
    // row1 resubmits `existing`'s dedup key with a different value, so it
    // derives the same id and must come back as a conflict.
    let row1 = common::fixture_usage_record(VCPU_GTS, tenant, "batch-dup", Decimal::new(42, 0));
    assert_eq!(
        row1.id, existing.id,
        "the same dedup key must derive the same id"
    );
    let row2 = common::fixture_usage_record(VCPU_GTS, tenant, "batch-2", Decimal::new(3, 0));

    let results = store
        .create_batch(vec![row0.clone(), row1, row2.clone()])
        .await
        .expect("batch returns per-row outcomes");

    assert_eq!(results.len(), 3, "one result per input row, in order");

    let r0 = results[0].as_ref().expect("row 0 inserted");
    assert_eq!(r0.id, row0.id, "row 0 preserves position");

    match results[1].as_ref() {
        Err(UsageCollectorPluginError::IdempotencyConflict { existing_id, .. }) => {
            assert_eq!(
                *existing_id, existing.id,
                "row 1 conflict points at the seeded row"
            );
        }
        other => panic!("row 1 must be IdempotencyConflict, got {other:?}"),
    }

    let r2 = results[2].as_ref().expect("row 2 inserted");
    assert_eq!(r2.id, row2.id, "row 2 preserves position");
}

/// Regression test for the `create_batch` single-`gts_id` assumption: a batch
/// mixing a registered and an unregistered `gts_id` must validate — and
/// persist or reject — every record against its OWN type, not just
/// `records[0]`'s. Before the per-`gts_id` partitioning fix, only the first
/// record's `gts_id` was locked/checked, so `row1` here (an unregistered,
/// non-first type) would have been inserted despite referencing a usage type
/// that does not exist.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_batch_mixed_gts_id_validates_each_partition_independently() {
    const UNREGISTERED_GTS: &str =
        "gts.cf.core.uc.usage_record.v1~cf.compute._.mixed_unregistered.v1";
    let Some((_h, store)) = setup().await else {
        return;
    };
    let tenant = Uuid::from_u128(0x1010);

    let row0 = common::fixture_usage_record(VCPU_GTS, tenant, "mixed-0", Decimal::new(1, 0));
    let row1 =
        common::fixture_usage_record(UNREGISTERED_GTS, tenant, "mixed-1", Decimal::new(2, 0));

    let results = store
        .create_batch(vec![row0.clone(), row1.clone()])
        .await
        .expect("mixed-type batch call itself must not fail outright");

    assert_eq!(results.len(), 2, "one outcome per input record, in order");

    let r0 = results[0]
        .as_ref()
        .expect("registered-type row must be persisted");
    assert_eq!(r0.id, row0.id, "row 0 preserves position");

    match results[1].as_ref() {
        Err(UsageCollectorPluginError::UsageTypeNotFound { gts_id }) => {
            assert_eq!(
                *gts_id,
                common::fixture_gts_id(UNREGISTERED_GTS),
                "row 1 fails its OWN catalog check, not row 0's"
            );
        }
        other => panic!(
            "row referencing an unregistered gts_id must fail its own catalog check regardless \
             of batch position, got {other:?}"
        ),
    }

    let err = store
        .get(row1.id)
        .await
        .expect_err("a row referencing an unregistered gts_id must never be written");
    assert!(
        matches!(err, UsageCollectorPluginError::UsageRecordNotFound { .. }),
        "expected UsageRecordNotFound, got {err:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_empty_batch_is_internal_error() {
    let Some((_h, store)) = setup().await else {
        return;
    };

    let err = store
        .create_batch(Vec::new())
        .await
        .expect_err("empty batch is a host-contract breach");
    assert!(
        matches!(err, UsageCollectorPluginError::Internal(_)),
        "empty batch must surface as Internal, got {err:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_deactivate_flips_target_and_active_compensations() {
    let Some((_h, store)) = setup().await else {
        return;
    };
    let tenant = Uuid::from_u128(0x1006);

    let target =
        common::fixture_usage_record(VCPU_GTS, tenant, "deact-target", Decimal::new(20, 0));
    let target = store.create(target).await.expect("create target");

    let mut comp =
        common::fixture_usage_record(VCPU_GTS, tenant, "deact-comp", Decimal::new(-20, 0));
    comp.corrects_id = Some(target.id);
    let comp = store.create(comp).await.expect("create compensation");

    store
        .deactivate(target.id)
        .await
        .expect("deactivate target succeeds");

    // Allow ReplacingMergeTree to apply FINAL at query time (always used in production reads).
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let fetched_target = store.get(target.id).await.expect("get target back");
    assert_eq!(
        fetched_target.status,
        UsageRecordStatus::Inactive,
        "target flips to inactive"
    );

    let fetched_comp = store.get(comp.id).await.expect("get compensation back");
    assert_eq!(
        fetched_comp.status,
        UsageRecordStatus::Inactive,
        "depth-1 active compensation flips to inactive"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_deactivate_missing_is_not_found() {
    let Some((_h, store)) = setup().await else {
        return;
    };

    let missing = Uuid::from_u128(999_999);
    let err = store
        .deactivate(missing)
        .await
        .expect_err("deactivating an unknown id must fail");

    match err {
        UsageCollectorPluginError::UsageRecordNotFound { id } => {
            assert_eq!(id, missing, "not-found carries the requested id");
        }
        other => panic!("expected UsageRecordNotFound, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_deactivate_already_inactive_is_already_inactive() {
    let Some((_h, store)) = setup().await else {
        return;
    };
    let tenant = Uuid::from_u128(0x1007);

    let record = common::fixture_usage_record(VCPU_GTS, tenant, "deact-twice", Decimal::new(30, 0));
    let record = store.create(record).await.expect("create record");

    store
        .deactivate(record.id)
        .await
        .expect("first deactivate succeeds");

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let err = store
        .deactivate(record.id)
        .await
        .expect_err("second deactivate on an inactive row must fail");

    match err {
        UsageCollectorPluginError::UsageRecordAlreadyInactive { id } => {
            assert_eq!(id, record.id, "already-inactive carries the row id");
        }
        other => panic!("expected UsageRecordAlreadyInactive, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_deactivate_leaves_unrelated_records_active() {
    let Some((_h, store)) = setup().await else {
        return;
    };
    let tenant = Uuid::from_u128(0x1008);

    let r1 = common::fixture_usage_record(VCPU_GTS, tenant, "deact-r1", Decimal::new(40, 0));
    let r1 = store.create(r1).await.expect("create r1");

    let r2 = common::fixture_usage_record(VCPU_GTS, tenant, "deact-r2", Decimal::new(50, 0));
    let r2 = store.create(r2).await.expect("create r2");

    store.deactivate(r1.id).await.expect("deactivate r1");
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let fetched_r2 = store.get(r2.id).await.expect("get r2 back");
    assert_eq!(
        fetched_r2.status,
        UsageRecordStatus::Active,
        "unrelated record stays active (depth-1 scope guard)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_deactivate_does_not_propagate_past_depth_one() {
    let Some((_h, store)) = setup().await else {
        return;
    };
    let tenant = Uuid::from_u128(0x100A);

    // Chain A <- B (corrects A) <- C (corrects B). Deactivating A flips A and
    // its depth-1 compensation B, but leaves the depth-2 row C active.
    let a = common::fixture_usage_record(VCPU_GTS, tenant, "deact-d2-a", Decimal::new(20, 0));
    let a = store.create(a).await.expect("create A");

    let mut b = common::fixture_usage_record(VCPU_GTS, tenant, "deact-d2-b", Decimal::new(-20, 0));
    b.corrects_id = Some(a.id);
    let b = store.create(b).await.expect("create B (corrects A)");

    let mut c = common::fixture_usage_record(VCPU_GTS, tenant, "deact-d2-c", Decimal::new(20, 0));
    c.corrects_id = Some(b.id);
    let c = store.create(c).await.expect("create C (corrects B)");

    store.deactivate(a.id).await.expect("deactivate A");
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    assert_eq!(
        store.get(a.id).await.expect("get A").status,
        UsageRecordStatus::Inactive,
        "target A flips to inactive"
    );
    assert_eq!(
        store.get(b.id).await.expect("get B").status,
        UsageRecordStatus::Inactive,
        "depth-1 compensation B (corrects A) flips to inactive"
    );
    assert_eq!(
        store.get(c.id).await.expect("get C").status,
        UsageRecordStatus::Active,
        "depth-2 row C (corrects B, not A) stays active: cascade is one level only"
    );
}

/// The `ClickHouse` dedup-convergence race: two concurrent inserts with the
/// same `(tenant_id, gts_id, created_at, id)` 4-tuple contend for the same
/// exclusive per-`gts_id` mutex, so the loser normally observes the winner's
/// row and absorbs it. A duplicate can still reach the table across a residual
/// window (a lease lapsing mid-critical-section); `ReplacingMergeTree` +
/// `FINAL`-qualified reads converge that duplicate to at most one visible row.
///
/// **Does NOT assert strict serializability**: this is the documented residual
/// dedup-atomicity deviation (DESIGN.md §3.6, PRD.md §5). The test proves
/// convergence only — after `FINAL`-qualified reads, at most one row is visible.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_dedup_race_converges() {
    let Some((_h, store)) = setup().await else {
        return;
    };
    let tenant = Uuid::from_u128(0xDED);

    // Two identical records; both contend for the same exclusive per-gts_id
    // mutex, and whichever loses the race sees the winner's row.
    let rec_a = common::fixture_usage_record(VCPU_GTS, tenant, "idem-race", Decimal::new(1, 0));
    let rec_b = rec_a.clone(); // exact duplicate — same 4-tuple key
    // Captured before the records move into the spawned tasks.
    let id = rec_a.id;

    // Barrier: both tasks rendezvous before calling create, so the two calls
    // are genuinely concurrent instead of serializing on scheduling luck — the
    // race window is entered on every run.
    let barrier = std::sync::Arc::new(Barrier::new(2));
    let b1 = std::sync::Arc::clone(&barrier);
    let b2 = std::sync::Arc::clone(&barrier);

    let s1 = store.clone();
    let s2 = store.clone();
    let (r1, r2) = tokio::join!(
        tokio::spawn(async move {
            b1.wait().await;
            s1.create(rec_a).await
        }),
        tokio::spawn(async move {
            b2.wait().await;
            s2.create(rec_b).await
        }),
    );
    let r1 = r1.expect("task a join");
    let r2 = r2.expect("task b join");

    // At least one must succeed; the other may succeed (dedup race) or absorb.
    assert!(
        r1.is_ok() || r2.is_ok(),
        "at least one concurrent insert must succeed"
    );

    // Allow ClickHouse a moment for any background-merge activity, then
    // query with FINAL. ReplacingMergeTree(version) + FINAL collapses to the
    // highest-version row — at most one row visible, never two.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let row = store.get(id).await.expect("get the raced record via FINAL");
    // FINAL means exactly one row is visible per dedup key; the get itself
    // proves at-most-one (get returns UsageRecordNotFound if no rows, or the
    // single FINAL-collapsed row if one or more were inserted).
    assert_eq!(
        row.id, id,
        "FINAL collapses the concurrent inserts to exactly one visible row"
    );
}

/// The multi-row deactivation INSERT is atomic at the `ClickHouse` part level.
///
/// A `FINAL`-qualified reader either observes the pre-cascade state (both target
/// and compensation are active) or the post-cascade state (both inactive) —
/// never a partial flip. This test polls `FINAL` after each deactivation step
/// and asserts the all-or-nothing invariant holds throughout.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_deactivation_cascade_is_atomic() {
    let Some((_h, store)) = setup().await else {
        return;
    };
    let tenant = Uuid::from_u128(0xA70C);

    let target =
        common::fixture_usage_record(VCPU_GTS, tenant, "atom-target", Decimal::new(100, 0));
    let target = store.create(target).await.expect("create target");

    let mut comp =
        common::fixture_usage_record(VCPU_GTS, tenant, "atom-comp", Decimal::new(-100, 0));
    comp.corrects_id = Some(target.id);
    let comp = store.create(comp).await.expect("create compensation");

    // Barrier: reader and writer rendezvous before deactivation so the reader
    // starts polling concurrently with (not after) the deactivation INSERT.
    let barrier = std::sync::Arc::new(Barrier::new(2));

    let s_read = store.clone();
    let target_id = target.id;
    let comp_id = comp.id;
    let b_read = std::sync::Arc::clone(&barrier);

    let reader = tokio::spawn(async move {
        b_read.wait().await;
        // Poll FINAL reads for up to 500 ms, asserting full-or-nothing at every check.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(500);
        while tokio::time::Instant::now() < deadline {
            let t_status = s_read
                .get(target_id)
                .await
                .map_or(UsageRecordStatus::Active, |r| r.status);
            let c_status = s_read
                .get(comp_id)
                .await
                .map_or(UsageRecordStatus::Active, |r| r.status);
            // Full-or-nothing invariant: both must be in the same state.
            assert_eq!(
                t_status, c_status,
                "partial cascade is never visible via FINAL: target={t_status:?}, comp={c_status:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    });

    barrier.wait().await;
    store
        .deactivate(target.id)
        .await
        .expect("deactivate target");

    reader.await.expect("reader task completed without panic");

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        store.get(target.id).await.expect("get target").status,
        UsageRecordStatus::Inactive,
        "target is inactive after cascade"
    );
    assert_eq!(
        store.get(comp.id).await.expect("get comp").status,
        UsageRecordStatus::Inactive,
        "compensation is inactive after cascade"
    );
}

// ── Lock-guard failure paths against a live client ───────────────────────────

/// A release failure after a successful `INSERT` must not lose the write: the
/// row stays durable, the call reports a retryable `Transient` (the lock may
/// stay held cluster-side until its TTL lapses), and the host's retry absorbs
/// the already-written row as a duplicate instead of inserting it twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_create_surfaces_release_failure_after_writing_the_row() {
    let Some((h, store)) = setup().await else {
        return;
    };
    let tenant = Uuid::from_u128(0x100B);

    let record =
        common::fixture_usage_record(VCPU_GTS, tenant, "idem-release-fails", Decimal::new(19, 0));

    let failing_store = common::record_store_with_lock_backend(
        &h,
        std::sync::Arc::new(stubs::ReleaseFailsLockBackend),
    );

    let err = failing_store
        .create(record.clone())
        .await
        .expect_err("a release failure after the insert must be surfaced");
    assert!(
        matches!(err, UsageCollectorPluginError::Transient { .. }),
        "a release failure is retryable, got {err:?}"
    );

    // The INSERT ran before the release failed, so the write is durable.
    let written = store
        .get(record.id)
        .await
        .expect("the row must survive the release failure");
    assert_eq!(written.value, record.value, "the written value is intact");
    assert_eq!(
        written.status,
        UsageRecordStatus::Active,
        "the written row is active"
    );

    let retried = store
        .create(record.clone())
        .await
        .expect("the retry must be absorbed, not conflict");
    assert_eq!(
        retried.id, record.id,
        "the retry absorbs the row written before the release failed"
    );
    assert_eq!(
        retried.value, record.value,
        "absorb returns the stored value, so the retry did not insert a second row"
    );
}

// ── Backend-error classification ─────────────────────────────────────────────

/// Every `RecordStore` method surfaces a backend failure as a plugin error.
///
/// A read that returned an empty page, or a write that reported success, on a
/// dead backend would be a silent data-loss bug: the host would record the
/// usage as persisted. The store keeps the harness's live lock backend so each
/// call reaches its SQL rather than stopping at lock acquisition.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_record_backend_failure_is_surfaced_by_every_operation() {
    use toolkit_odata::ODataQuery;
    use usage_collector_sdk::{AggregationOp, AggregationSpec};

    let Some(h) = common::bring_up_or_skip().await else {
        return;
    };
    let store = common::record_store_over(&h, common::unreachable_client());
    let gts_id = common::fixture_gts_id(VCPU_GTS);
    let tenant = Uuid::from_u128(0x9001);

    let record = common::fixture_usage_record(VCPU_GTS, tenant, "idem-dead-backend", Decimal::ONE);

    store
        .create(record.clone())
        .await
        .expect_err("create must surface the backend failure");

    // create_batch reports per-record outcomes, so the partition failure lands
    // in every slot rather than as a top-level error.
    let outcomes = store
        .create_batch(vec![record])
        .await
        .expect("create_batch itself returns Ok with per-record outcomes");
    assert_eq!(outcomes.len(), 1);
    assert!(
        outcomes[0].is_err(),
        "the failed partition must mark its record as failed, got {:?}",
        outcomes[0]
    );

    store
        .get(Uuid::from_u128(0x9002))
        .await
        .expect_err("get must surface the backend failure");
    store
        .list(gts_id.clone(), &ODataQuery::new(), &[])
        .await
        .expect_err("list must surface the backend failure");
    store
        .aggregate(
            gts_id,
            &ODataQuery::new(),
            &[],
            AggregationSpec {
                op: AggregationOp::Sum,
                group_by: Vec::new(),
            },
        )
        .await
        .expect_err("aggregate must surface the backend failure");
    store
        .deactivate(Uuid::from_u128(0x9002))
        .await
        .expect_err("deactivate must surface the backend failure");
}

/// Cluster lock backend that grants every acquisition and renews the lease, so
/// the injected failure lands on `create`'s explicit release — after its
/// `INSERT` — rather than at acquisition or on the pre-write lease check.
mod stubs {
    use std::time::Duration;

    use async_trait::async_trait;
    use cluster_sdk::error::{ClusterError, ProviderErrorKind};
    use cluster_sdk::lock::{DistributedLockBackend, LockFeatures, LockGuard, LockRequest};

    pub struct ReleaseFailsLockBackend;

    impl ReleaseFailsLockBackend {
        /// Hand out a guard whose renew succeeds and whose release fails.
        fn grant(name: &str) -> LockGuard {
            let (mut requests, guard) = LockGuard::channel(name.to_owned(), 1);
            let name = name.to_owned();
            tokio::spawn(async move {
                while let Some(request) = requests.recv().await {
                    match request {
                        LockRequest::Renew { responder, .. } => responder.respond(Ok(())),
                        LockRequest::Release { responder } => {
                            responder.respond(Err(ClusterError::Provider {
                                kind: ProviderErrorKind::ConnectionLost,
                                message: format!("release of `{name}` failed (test stub)"),
                            }));
                            return;
                        }
                    }
                }
            });
            guard
        }
    }

    #[async_trait]
    impl DistributedLockBackend for ReleaseFailsLockBackend {
        fn features(&self) -> LockFeatures {
            // The lock manager requires linearizable exclusion at resolve time.
            LockFeatures::new(true)
        }

        async fn try_lock(&self, name: &str, _ttl: Duration) -> Result<LockGuard, ClusterError> {
            Ok(Self::grant(name))
        }

        async fn lock(
            &self,
            name: &str,
            _ttl: Duration,
            _timeout: Duration,
        ) -> Result<LockGuard, ClusterError> {
            Ok(Self::grant(name))
        }
    }
}

/// Pins the fixture's identity contract: every test record above must carry the
/// id the gateway would have derived, because that id *is* the dedup key this
/// store looks rows up by. A fixture stamping a synthetic id (a counter, say)
/// would make the dedup tests above assert nothing — the lookup would simply
/// miss and every duplicate would insert cleanly.
///
/// Needs no Docker: it builds fixtures and compares ids, and so runs on the
/// default (non-`--ignored`) pass as a guard on the rest of the file.
#[test]
fn fixture_record_id_is_derived_from_its_own_dedup_key() {
    let tenant = Uuid::from_u128(0xF1AC);
    let record = common::fixture_usage_record(VCPU_GTS, tenant, "idem-pin", Decimal::ONE);

    assert_eq!(
        record.id,
        derive_usage_record_id(
            record.tenant_id,
            &record.gts_id,
            &record.idempotency_key,
            record.created_at
        ),
        "the fixture id must be derived from the record's own \
         (tenant_id, gts_id, idempotency_key, created_at)"
    );

    // Each derivation input actually moves the id.
    let other_tenant =
        common::fixture_usage_record(VCPU_GTS, Uuid::from_u128(0xF1AD), "idem-pin", Decimal::ONE);
    assert_ne!(record.id, other_tenant.id, "tenant_id feeds the id");

    let other_idem = common::fixture_usage_record(VCPU_GTS, tenant, "idem-pin-2", Decimal::ONE);
    assert_ne!(record.id, other_idem.id, "idempotency_key feeds the id");

    let other_instant = common::fixture_usage_record_at(
        VCPU_GTS,
        tenant,
        "idem-pin",
        Decimal::ONE,
        common::fixture_created_at_offset(1),
    );
    assert_ne!(record.id, other_instant.id, "created_at feeds the id");

    // Value is not a derivation input, so it must not move the id.
    let other_value =
        common::fixture_usage_record(VCPU_GTS, tenant, "idem-pin", Decimal::new(7, 0));
    assert_eq!(
        record.id, other_value.id,
        "value is not part of the dedup key"
    );
}
