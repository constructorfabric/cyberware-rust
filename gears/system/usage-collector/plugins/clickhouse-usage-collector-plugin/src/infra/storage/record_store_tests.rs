// Test modules using bare `panic!` opt in explicitly.
#![allow(clippy::panic)]

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use rust_decimal::Decimal;
use uuid::Uuid;

use usage_collector_sdk::{UsageCollectorPluginError, UsageRecord, UsageTypeGtsId};

use super::{
    AggregateNdjsonParser, ChRecordStore, err_for_partition, parse_aggregate_response,
    prefer_dedup_row, record_dedup_key, row_dedup_key,
};
use crate::domain::ports::RecordStore;
use crate::infra::coordination::lock_manager::LockGuardPort;
use crate::infra::metrics::Metrics;
use crate::infra::storage::catalog_store::CatalogLockPort;
use crate::infra::storage::entity::{UsageRecordRow, UsageRecordStatusCode};
use crate::infra::storage::mapper::{canonical_equal, version_higher_than};

const VCPU_GTS: &str = "gts.cf.core.uc.usage_record.v1~cf.compute._.vcpu_hours.v1";

fn make_row(id: Uuid, tenant_id: Uuid, created_at_micros: i64, version: u64) -> UsageRecordRow {
    UsageRecordRow {
        id,
        tenant_id,
        gts_id: VCPU_GTS.to_owned(),
        value: Decimal::new(100, 0),
        created_at: created_at_micros,
        resource_id: "res-1".to_owned(),
        resource_type: "vm".to_owned(),
        subject_id: None,
        subject_type: None,
        idempotency_key: "idem-1".to_owned(),
        corrects_id: None,
        status: UsageRecordStatusCode::Active,
        metadata: HashMap::new(),
        ingested_at: created_at_micros,
        version,
    }
}

// ── dedup key helpers ─────────────────────────────────────────────────────────

#[test]
fn row_dedup_key_matches_record_dedup_key_for_same_tuple() {
    use time::OffsetDateTime;
    use usage_collector_sdk::{IdempotencyKey, ResourceRef, UsageRecord, UsageRecordStatus};

    let id = Uuid::from_u128(1);
    let tenant_id = Uuid::from_u128(2);
    let created_at_micros = 1_700_000_000_000_000_i64;

    let row = make_row(id, tenant_id, created_at_micros, 1);

    let gts_id = UsageTypeGtsId::new(VCPU_GTS).unwrap();
    let created_at =
        OffsetDateTime::from_unix_timestamp_nanos(i128::from(created_at_micros) * 1_000).unwrap();
    let record = UsageRecord {
        id,
        tenant_id,
        gts_id,
        value: Decimal::new(100, 0),
        created_at,
        resource_ref: ResourceRef::new("res-1".to_owned(), "vm".to_owned()).unwrap(),
        subject_ref: None,
        idempotency_key: IdempotencyKey::new("idem-1".to_owned()).unwrap(),
        corrects_id: None,
        status: UsageRecordStatus::Active,
        metadata: std::collections::BTreeMap::default(),
    };

    assert_eq!(row_dedup_key(&row), record_dedup_key(&record));
}

// ── canonical_equal ───────────────────────────────────────────────────────────

#[test]
fn canonical_equal_returns_true_for_exact_match() {
    use time::OffsetDateTime;
    use usage_collector_sdk::{IdempotencyKey, ResourceRef, UsageRecord, UsageRecordStatus};

    let id = Uuid::from_u128(42);
    let tenant_id = Uuid::from_u128(99);
    let created_at_micros = 1_700_000_000_000_000_i64;
    let row = make_row(id, tenant_id, created_at_micros, 1);

    let gts_id = UsageTypeGtsId::new(VCPU_GTS).unwrap();
    let created_at =
        OffsetDateTime::from_unix_timestamp_nanos(i128::from(created_at_micros) * 1_000).unwrap();
    let record = UsageRecord {
        id,
        tenant_id,
        gts_id,
        value: Decimal::new(100, 0),
        created_at,
        resource_ref: ResourceRef::new("res-1".to_owned(), "vm".to_owned()).unwrap(),
        subject_ref: None,
        idempotency_key: IdempotencyKey::new("idem-1".to_owned()).unwrap(),
        corrects_id: None,
        status: UsageRecordStatus::Active,
        metadata: std::collections::BTreeMap::default(),
    };

    assert!(canonical_equal(&row, &record).unwrap());
}

#[test]
fn canonical_equal_returns_false_when_value_differs() {
    use time::OffsetDateTime;
    use usage_collector_sdk::{IdempotencyKey, ResourceRef, UsageRecord, UsageRecordStatus};

    let id = Uuid::from_u128(42);
    let tenant_id = Uuid::from_u128(99);
    let created_at_micros = 1_700_000_000_000_000_i64;
    let row = make_row(id, tenant_id, created_at_micros, 1);

    let gts_id = UsageTypeGtsId::new(VCPU_GTS).unwrap();
    let created_at =
        OffsetDateTime::from_unix_timestamp_nanos(i128::from(created_at_micros) * 1_000).unwrap();
    let record = UsageRecord {
        id,
        tenant_id,
        gts_id,
        value: Decimal::new(999, 0), // differs
        created_at,
        resource_ref: ResourceRef::new("res-1".to_owned(), "vm".to_owned()).unwrap(),
        subject_ref: None,
        idempotency_key: IdempotencyKey::new("idem-1".to_owned()).unwrap(),
        corrects_id: None,
        status: UsageRecordStatus::Active,
        metadata: std::collections::BTreeMap::default(),
    };

    assert!(!canonical_equal(&row, &record).unwrap());
}

// ── version_higher_than ───────────────────────────────────────────────────────

#[test]
fn version_higher_than_exceeds_existing() {
    let existing = 100_u64;
    assert!(version_higher_than(existing, 0) > existing);
}

#[test]
fn version_higher_than_with_offset_provides_headroom() {
    let existing = 100_u64;
    let offset = 5_u64;
    assert!(version_higher_than(existing, offset) > existing.saturating_add(offset));
}

// ── UsageRecord::try_from(UsageRecordRow) ─────────────────────────────────────
//
// A malformed `status` can no longer be represented in a `UsageRecordRow` --
// `UsageRecordStatusCode` is a closed `#[repr(i8)]` enum, so any Enum8
// discriminant outside {Active = 1, Inactive = 2} is rejected by the
// `clickhouse` crate's own schema validation before a row is ever
// constructed here (see `entity.rs` / `From<UsageRecordStatusCode>`).

#[test]
fn row_with_invalid_gts_id_maps_to_internal() {
    let mut row = make_row(Uuid::new_v4(), Uuid::new_v4(), 1_700_000_000_000_000, 1);
    row.gts_id = "not-valid".to_owned();
    assert!(matches!(
        UsageRecord::try_from(row),
        Err(UsageCollectorPluginError::Internal(_))
    ));
}

// ── batch dedup key uniqueness ────────────────────────────────────────────────

#[test]
fn two_distinct_rows_produce_distinct_dedup_keys() {
    let row1 = make_row(Uuid::from_u128(1), Uuid::from_u128(10), 1_000_000, 1);
    let mut row2 = make_row(Uuid::from_u128(2), Uuid::from_u128(10), 1_000_000, 1);
    row2.idempotency_key = "idem-2".to_owned();
    assert_ne!(row_dedup_key(&row1), row_dedup_key(&row2));
}

#[test]
fn same_tuple_produces_same_dedup_key() {
    let row1 = make_row(Uuid::from_u128(5), Uuid::from_u128(10), 2_000_000, 1);
    let row2 = make_row(Uuid::from_u128(5), Uuid::from_u128(10), 2_000_000, 2); // version differs
    assert_eq!(row_dedup_key(&row1), row_dedup_key(&row2));
}

/// The dedup key is the SPI's canonical tuple, not the record `id`. Keying on
/// `id` is what let a row whose `id` disagreed with its own canonical tuple slip
/// past the lookup and be re-inserted under an idempotency key already in use.
#[test]
fn dedup_key_excludes_id_and_includes_idempotency_key() {
    let tenant_id = Uuid::from_u128(10);
    let created_at_micros = 3_000_000_i64;

    let row = make_row(Uuid::from_u128(1), tenant_id, created_at_micros, 1);

    let mut mismatched_id = make_row(Uuid::from_u128(2), tenant_id, created_at_micros, 1);
    assert_eq!(
        row_dedup_key(&row),
        row_dedup_key(&mismatched_id),
        "a differing `id` must not move the dedup key, or the lookup misses the stored row"
    );

    mismatched_id.idempotency_key = "idem-2".to_owned();
    assert_ne!(
        row_dedup_key(&row),
        row_dedup_key(&mismatched_id),
        "a differing `idempotency_key` is a different dedup identity"
    );

    // The incoming-record projection agrees with the stored-row one.
    let record = make_record(Uuid::from_u128(3), tenant_id, created_at_micros);
    assert_eq!(
        row_dedup_key(&row),
        record_dedup_key(&record),
        "`record_dedup_key` must project the same tuple, independent of `id`"
    );
}

// ── prefer_dedup_row ──────────────────────────────────────────────────────────
//
// `ClickHouse` has no UNIQUE constraint, so rows written while the lookup was
// keyed on `id` can leave two rows sharing a dedup key with different `id`s.
// Both survive `FINAL`, so the lookup must choose between them deterministically
// and in favour of the caller's own record.

#[test]
fn prefer_dedup_row_takes_the_only_candidate() {
    let row = make_row(Uuid::from_u128(9), Uuid::from_u128(10), 1_000_000, 1);
    let chosen = prefer_dedup_row(None, row, Uuid::from_u128(9));
    assert_eq!(chosen.id, Uuid::from_u128(9));
}

#[test]
fn prefer_dedup_row_prefers_the_expected_id_over_a_lower_id() {
    let tenant_id = Uuid::from_u128(10);
    let expected = Uuid::from_u128(5);
    let twin = make_row(Uuid::from_u128(1), tenant_id, 1_000_000, 1);
    let honest = make_row(expected, tenant_id, 1_000_000, 1);

    // Whichever order the rows arrive in, the expected `id` wins — even though
    // the twin sorts lower.
    assert_eq!(
        prefer_dedup_row(Some(twin.clone()), honest.clone(), expected).id,
        expected
    );
    assert_eq!(
        prefer_dedup_row(Some(honest), twin, expected).id,
        expected,
        "an already-chosen exact match must not be displaced by a lower-`id` twin"
    );
}

#[test]
fn prefer_dedup_row_falls_back_to_the_lowest_id() {
    let tenant_id = Uuid::from_u128(10);
    let absent = Uuid::from_u128(99);
    let low = make_row(Uuid::from_u128(1), tenant_id, 1_000_000, 1);
    let high = make_row(Uuid::from_u128(2), tenant_id, 1_000_000, 1);

    // Order-independent when neither candidate is the expected `id`, so the
    // outcome never depends on which part ClickHouse read first.
    assert_eq!(
        prefer_dedup_row(Some(low.clone()), high.clone(), absent).id,
        Uuid::from_u128(1)
    );
    assert_eq!(
        prefer_dedup_row(Some(high), low, absent).id,
        Uuid::from_u128(1)
    );
}

// ── err_for_partition ─────────────────────────────────────────────────────────
//
// `UsageCollectorPluginError` is deliberately not `Clone`, so `create_batch`
// rebuilds an equivalent value per variant to place in every slot of a failed
// `gts_id` partition. A variant that loses its payload here would downgrade a
// caller-visible outcome (e.g. a retryable `Transient` becoming an opaque
// `Internal`), so each arm is pinned.

#[test]
fn err_for_partition_preserves_transient_payload() {
    let src = UsageCollectorPluginError::Transient {
        detail: "backend unreachable".to_owned(),
        retry_after_seconds: Some(7),
    };
    match err_for_partition(&src) {
        UsageCollectorPluginError::Transient {
            detail,
            retry_after_seconds,
        } => {
            assert_eq!(detail, "backend unreachable");
            assert_eq!(
                retry_after_seconds,
                Some(7),
                "retry_after_seconds must survive so the host can honour the backoff"
            );
        }
        other => panic!("expected Transient, got {other:?}"),
    }
}

#[test]
fn err_for_partition_preserves_usage_type_not_found_gts_id() {
    let gts_id = UsageTypeGtsId::new(VCPU_GTS).unwrap();
    let src = UsageCollectorPluginError::UsageTypeNotFound {
        gts_id: gts_id.clone(),
    };
    match err_for_partition(&src) {
        UsageCollectorPluginError::UsageTypeNotFound { gts_id: got } => assert_eq!(got, gts_id),
        other => panic!("expected UsageTypeNotFound, got {other:?}"),
    }
}

#[test]
fn err_for_partition_preserves_internal_message() {
    let src = UsageCollectorPluginError::Internal("dedup lookup exploded".to_owned());
    match err_for_partition(&src) {
        UsageCollectorPluginError::Internal(msg) => assert_eq!(msg, "dedup lookup exploded"),
        other => panic!("expected Internal, got {other:?}"),
    }
}

/// The enum is `#[non_exhaustive]`, so an unmodelled variant must still degrade
/// to an `Internal` carrying the original text rather than being dropped.
#[test]
fn err_for_partition_falls_back_to_internal_for_other_variants() {
    let src = UsageCollectorPluginError::IdempotencyConflict {
        idempotency_key: "idem-1".to_owned(),
        existing_id: Uuid::from_u128(7),
    };
    let rendered = src.to_string();
    match err_for_partition(&src) {
        UsageCollectorPluginError::Internal(msg) => assert_eq!(
            msg, rendered,
            "the fallback arm must carry the original error text"
        ),
        other => panic!("expected Internal fallback, got {other:?}"),
    }
}

// ── push_metadata_filters ─────────────────────────────────────────────────────

/// Both the key and every value are bound, so no caller-supplied metadata text
/// reaches the SQL string.
///
/// The `values.is_empty()` -> `FALSE` arm of `push_metadata_filters` is not
/// covered: `MetadataFilter`'s fields are private and both `new` and its
/// `Deserialize` impl reject an empty value set, so an empty filter cannot be
/// constructed to pass in.
#[test]
fn metadata_filter_binds_key_and_every_value() {
    use usage_collector_sdk::MetadataFilter;

    use crate::infra::storage::query::translate::{SqlBind, SqlCtx};

    let filter = MetadataFilter::new("region", ["eu-west", "us-east"]).unwrap();
    let mut ctx = SqlCtx::new();
    let mut clauses = Vec::new();
    ChRecordStore::push_metadata_filters(std::slice::from_ref(&filter), &mut ctx, &mut clauses);

    assert_eq!(clauses, vec!["metadata[?] IN (?, ?)".to_owned()]);
    assert_eq!(ctx.binds.len(), 3, "one key bind plus one bind per value");
    assert!(matches!(&ctx.binds[0], SqlBind::Str(s) if s == "region"));
}

// ── Empty-input short circuits ────────────────────────────────────────────────

// ── Lock stubs ────────────────────────────────────────────────────────────────

/// Guard stub that always reports the lease as still held.
struct GrantedGuard;

#[async_trait]
impl LockGuardPort for GrantedGuard {
    async fn ensure_still_held(&self) -> Result<(), UsageCollectorPluginError> {
        Ok(())
    }

    async fn release(self: Box<Self>) -> Result<(), UsageCollectorPluginError> {
        Ok(())
    }
}

/// Lock stub that always grants the exclusive lock immediately.
struct AlwaysGrantLock;

#[async_trait]
impl CatalogLockPort for AlwaysGrantLock {
    async fn acquire_exclusive_for_delete(
        &self,
        _gts_id: &str,
    ) -> Result<Box<dyn LockGuardPort>, UsageCollectorPluginError> {
        Ok(Box::new(GrantedGuard))
    }

    // Overridden explicitly rather than inherited from the trait default, so a
    // create-path test asserts against this stub and not the default's
    // delegation to `acquire_exclusive_for_delete`.
    async fn acquire_exclusive_for_create(
        &self,
        _gts_id: &str,
    ) -> Result<Box<dyn LockGuardPort>, UsageCollectorPluginError> {
        Ok(Box::new(GrantedGuard))
    }
}

/// Lock stub that never grants the lock — the cluster lock manager being
/// unavailable at acquisition time.
struct AlwaysTransientLock;

#[async_trait]
impl CatalogLockPort for AlwaysTransientLock {
    async fn acquire_exclusive_for_delete(
        &self,
        _gts_id: &str,
    ) -> Result<Box<dyn LockGuardPort>, UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::transient(
            "cluster lock unavailable (test stub)",
        ))
    }

    // See `AlwaysGrantLock`: overridden explicitly so the create path is
    // exercised against this stub rather than the trait default.
    async fn acquire_exclusive_for_create(
        &self,
        _gts_id: &str,
    ) -> Result<Box<dyn LockGuardPort>, UsageCollectorPluginError> {
        Err(UsageCollectorPluginError::transient(
            "cluster lock unavailable (test stub)",
        ))
    }
}

/// Build a store over an offline client and a caller-chosen lock stub.
///
/// Port 1 is reserved and never bound, so any query that is actually issued
/// fails fast (connection refused) instead of blocking. The `clickhouse` crate's
/// default address (`http://localhost:8123`) would let a real local server
/// answer these "offline" tests.
fn store_with_lock(lock: Arc<dyn CatalogLockPort>) -> ChRecordStore {
    ChRecordStore::new(
        clickhouse::Client::default().with_url("http://127.0.0.1:1"),
        lock,
        Arc::new(Metrics::new()),
        // Generous: every assertion here either short-circuits before I/O or
        // fails fast on connection refused, so the deadline is never what a
        // test observes.
        std::time::Duration::from_secs(30),
    )
}

/// Build a store over an offline client. Both assertions below short-circuit
/// before any I/O, so no server is required.
fn offline_store() -> ChRecordStore {
    store_with_lock(Arc::new(AlwaysGrantLock))
}

/// Build a `UsageRecord` matching [`make_row`]'s canonical fields.
fn make_record(id: Uuid, tenant_id: Uuid, created_at_micros: i64) -> UsageRecord {
    use time::OffsetDateTime;
    use usage_collector_sdk::{IdempotencyKey, ResourceRef, UsageRecordStatus};

    UsageRecord {
        id,
        tenant_id,
        gts_id: UsageTypeGtsId::new(VCPU_GTS).unwrap(),
        value: Decimal::new(100, 0),
        created_at: OffsetDateTime::from_unix_timestamp_nanos(
            i128::from(created_at_micros) * 1_000,
        )
        .unwrap(),
        resource_ref: ResourceRef::new("res-1".to_owned(), "vm".to_owned()).unwrap(),
        subject_ref: None,
        idempotency_key: IdempotencyKey::new("idem-1".to_owned()).unwrap(),
        corrects_id: None,
        status: UsageRecordStatus::Active,
        metadata: std::collections::BTreeMap::default(),
    }
}

/// An all-absorbed / all-rejected batch leaves nothing to write; the insert must
/// then be skipped rather than sending an empty INSERT to `ClickHouse`.
#[tokio::test]
async fn insert_records_with_no_rows_is_a_no_op() {
    offline_store()
        .insert_records(&[], std::time::Instant::now())
        .await
        .expect("an empty row set must not touch the backend");
}

#[tokio::test]
async fn batch_dedup_lookup_with_no_records_returns_empty_map() {
    let found = offline_store()
        .batch_dedup_lookup(&[])
        .await
        .expect("an empty record set must not touch the backend");
    assert!(found.is_empty());
}

// ── Dedup-hit resolution ──────────────────────────────────────────────────────
//
// `canonical_equal` deliberately excludes `status`, so absorbing purely on it
// would let a create -> deactivate -> re-create of the same dedup key return
// `Ok` carrying the *inactive* stored row. The key is already bound to a
// record the caller cannot have back, which is an idempotency conflict.

#[test]
fn dedup_hit_on_an_inactive_stored_row_is_an_idempotency_conflict() {
    let id = Uuid::from_u128(7);
    let tenant_id = Uuid::from_u128(8);
    let created_at_micros = 1_700_000_000_000_000_i64;

    let mut row = make_row(id, tenant_id, created_at_micros, 1);
    row.status = UsageRecordStatusCode::Inactive;
    let record = make_record(id, tenant_id, created_at_micros);

    assert!(
        canonical_equal(&row, &record).unwrap(),
        "the canonical fields must match, so only `status` can drive the rejection"
    );

    match offline_store().resolve_dedup_hit(&row, &record) {
        Err(UsageCollectorPluginError::IdempotencyConflict { existing_id, .. }) => {
            assert_eq!(existing_id, id);
        }
        other => panic!("expected IdempotencyConflict, got {other:?}"),
    }
}

/// A stored row sharing the canonical dedup tuple but carrying a different `id`
/// is a corrupted identity: the key is already bound to a record the caller
/// cannot address, so it must fail closed rather than absorb.
///
/// This test is only meaningful because the lookup keys on the canonical tuple
/// ([`super::DedupKey`]) rather than on `id` — an `id`-keyed lookup would never
/// surface this row at all, and the create would silently insert a duplicate.
#[test]
fn dedup_hit_on_a_mismatched_id_row_is_an_idempotency_conflict() {
    let stored_id = Uuid::from_u128(7);
    let incoming_id = Uuid::from_u128(8);
    let tenant_id = Uuid::from_u128(9);
    let created_at_micros = 1_700_000_000_000_000_i64;

    let row = make_row(stored_id, tenant_id, created_at_micros, 1);
    let record = make_record(incoming_id, tenant_id, created_at_micros);

    assert_eq!(
        row_dedup_key(&row),
        record_dedup_key(&record),
        "the rows must share a dedup key, so only the mismatched `id` can drive the rejection"
    );
    assert_eq!(
        row.status,
        UsageRecordStatusCode::Active,
        "the stored row must be active, so `status` cannot drive the rejection either"
    );

    match offline_store().resolve_dedup_hit(&row, &record) {
        Err(UsageCollectorPluginError::IdempotencyConflict {
            existing_id,
            idempotency_key,
        }) => {
            assert_eq!(existing_id, stored_id, "the conflict names the stored row");
            assert_eq!(idempotency_key, "idem-1");
        }
        other => panic!("expected IdempotencyConflict, got {other:?}"),
    }
}

#[test]
fn dedup_hit_on_an_identical_active_row_is_absorbed() {
    let id = Uuid::from_u128(7);
    let tenant_id = Uuid::from_u128(8);
    let created_at_micros = 1_700_000_000_000_000_i64;

    let row = make_row(id, tenant_id, created_at_micros, 1);
    let record = make_record(id, tenant_id, created_at_micros);

    let absorbed = offline_store()
        .resolve_dedup_hit(&row, &record)
        .expect("an identical active row must be absorbed");
    assert_eq!(absorbed.id, id);
}

/// A second row for a dedup key already composed *within the same batch* goes
/// through the same comparison as one already stored, so a conflicting
/// in-batch duplicate is reported rather than silently swallowed.
#[test]
fn conflicting_in_batch_duplicate_is_an_idempotency_conflict() {
    let id = Uuid::from_u128(7);
    let tenant_id = Uuid::from_u128(8);
    let created_at_micros = 1_700_000_000_000_000_i64;

    let composed = make_row(id, tenant_id, created_at_micros, 1);
    let mut conflicting = make_record(id, tenant_id, created_at_micros);
    conflicting.value = Decimal::new(999, 0);

    assert_eq!(
        record_dedup_key(&conflicting),
        row_dedup_key(&composed),
        "the two rows must share a dedup key for this to be an in-batch duplicate"
    );

    match offline_store().resolve_dedup_hit(&composed, &conflicting) {
        Err(UsageCollectorPluginError::IdempotencyConflict { existing_id, .. }) => {
            assert_eq!(existing_id, id);
        }
        other => panic!("expected IdempotencyConflict, got {other:?}"),
    }
}

// ── Fail-closed create paths ──────────────────────────────────────────────────
//
// DESIGN.md §3.6 step 7: an unavailable coordination lock must never let a
// create proceed unlocked. Both entry points are covered because they acquire
// the lock independently.

#[tokio::test]
async fn create_returns_transient_when_the_lock_is_unavailable() {
    let store = store_with_lock(Arc::new(AlwaysTransientLock));
    let record = make_record(
        Uuid::from_u128(1),
        Uuid::from_u128(2),
        1_700_000_000_000_000,
    );

    let err = store
        .create(record)
        .await
        .expect_err("create must fail when the lock manager is unavailable");

    assert!(
        matches!(err, UsageCollectorPluginError::Transient { .. }),
        "expected a retryable Transient, got {err:?}"
    );
}

#[tokio::test]
async fn create_batch_reports_transient_per_record_when_the_lock_is_unavailable() {
    let store = store_with_lock(Arc::new(AlwaysTransientLock));
    let records = vec![
        make_record(
            Uuid::from_u128(1),
            Uuid::from_u128(2),
            1_700_000_000_000_000,
        ),
        make_record(
            Uuid::from_u128(3),
            Uuid::from_u128(2),
            1_700_000_000_000_001,
        ),
    ];

    let outcomes = store
        .create_batch(records)
        .await
        .expect("a denied lock is a per-record outcome, not a batch-level failure");

    assert_eq!(outcomes.len(), 2);
    for outcome in outcomes {
        match outcome {
            Err(UsageCollectorPluginError::Transient { .. }) => {}
            other => panic!("expected Transient per record, got {other:?}"),
        }
    }
}

/// A backend failure reached with the lock in hand is still a per-record
/// outcome, never a batch-level `Err` that discards the whole submission.
///
/// The failure here comes from the unreachable server, so its classification
/// is whatever the client reports for a refused connection; the contract under
/// test is the per-record shape of the result, not the variant.
#[tokio::test]
async fn create_batch_reports_backend_failures_per_record() {
    let store = store_with_lock(Arc::new(AlwaysGrantLock));
    let records = vec![make_record(
        Uuid::from_u128(1),
        Uuid::from_u128(2),
        1_700_000_000_000_000,
    )];

    let outcomes = store
        .create_batch(records)
        .await
        .expect("a backend failure inside the batch is a per-record outcome");

    assert_eq!(outcomes.len(), 1);
    match outcomes.into_iter().next() {
        Some(Err(
            UsageCollectorPluginError::Transient { .. }
            | UsageCollectorPluginError::Internal { .. },
        )) => {}
        other => panic!("expected a per-record backend failure, got {other:?}"),
    }
}

/// The batch INSERT runs after the dedup SELECTs have already decided every
/// record's outcome. A failed write must therefore rewrite only the slots that
/// were waiting on it — the rows absorbed from storage keep their `Ok`, since
/// a write that never landed cannot invalidate a row that was already there.
///
/// (Driving a real SELECT-ok / INSERT-fail sequence end to end needs a live
/// `ClickHouse` that accepts the reads and rejects the write, which the
/// feature-gated suite covers; the mapping itself is asserted here.)
#[test]
fn a_failed_insert_rewrites_only_the_slots_it_backed() {
    use super::apply_insert_failure;

    let absorbed = make_record(
        Uuid::from_u128(1),
        Uuid::from_u128(2),
        1_700_000_000_000_000,
    );
    let mut outcomes: Vec<Option<Result<UsageRecord, UsageCollectorPluginError>>> = vec![
        Some(Ok(absorbed)),
        Some(Ok(make_record(
            Uuid::from_u128(3),
            Uuid::from_u128(2),
            1_700_000_000_000_001,
        ))),
        Some(Ok(make_record(
            Uuid::from_u128(4),
            Uuid::from_u128(2),
            1_700_000_000_000_002,
        ))),
    ];
    // Slot 0 was absorbed from storage; slots 1 and 2 share one composed row
    // (an identical in-batch duplicate), so both hang off the same write.
    let row_slots = vec![vec![1_usize, 2_usize]];

    apply_insert_failure(
        &UsageCollectorPluginError::transient("insert failed (test stub)"),
        &row_slots,
        &mut outcomes,
    );

    assert!(
        matches!(outcomes[0], Some(Ok(_))),
        "an absorbed row must keep the outcome the dedup read decided"
    );
    for idx in [1, 2] {
        match &outcomes[idx] {
            Some(Err(UsageCollectorPluginError::Transient { .. })) => {}
            other => panic!("slot {idx} must carry the insert failure, got {other:?}"),
        }
    }
}

// ── parse_aggregate_response ──────────────────────────────────────────────────

#[test]
fn aggregate_response_parses_dimensions_and_decimal_values() {
    let dim_names = vec!["d0".to_owned(), "d1".to_owned()];
    let body = b"{\"d0\":\"tenant-a\",\"d1\":\"vm\",\"agg\":\"12.5\"}\n\
                 {\"d0\":\"tenant-b\",\"d1\":\"disk\",\"agg\":7}\n";

    let buckets = parse_aggregate_response(body, &dim_names).expect("well-formed NDJSON");

    assert_eq!(buckets.len(), 2);
    assert_eq!(buckets[0].key, vec!["tenant-a", "vm"]);
    assert_eq!(
        buckets[0].value.as_ref().map(ToString::to_string),
        Some("12.5".to_owned())
    );
    assert_eq!(buckets[1].key, vec!["tenant-b", "disk"]);
    assert_eq!(
        buckets[1].value.as_ref().map(ToString::to_string),
        Some("7".to_owned())
    );
}

/// An empty `MIN`/`MAX`/`AVG` group comes back as JSON `null`, which is a
/// valid absent value rather than a parse failure. Blank lines between rows
/// are skipped, and a missing dimension key decodes as an empty component.
#[test]
fn aggregate_response_handles_null_values_blank_lines_and_missing_dimensions() {
    let dim_names = vec!["d0".to_owned()];
    let body = b"{\"d0\":\"tenant-a\",\"agg\":null}\n\n{\"agg\":\"3\"}\n";

    let buckets = parse_aggregate_response(body, &dim_names).expect("well-formed NDJSON");

    assert_eq!(buckets.len(), 2, "the blank line must be skipped");
    assert_eq!(buckets[0].key, vec!["tenant-a"]);
    assert!(buckets[0].value.is_none());
    assert_eq!(buckets[1].key, vec![String::new()]);
}

#[test]
fn aggregate_response_with_an_ungrouped_query_yields_one_keyless_bucket() {
    let buckets = parse_aggregate_response(b"{\"agg\":\"42\"}\n", &[]).expect("well-formed NDJSON");
    assert_eq!(buckets.len(), 1);
    assert!(buckets[0].key.is_empty());
}

#[test]
fn aggregate_response_rejects_malformed_json() {
    let err = parse_aggregate_response(b"{not json}\n", &[])
        .expect_err("a malformed line must not be silently dropped");
    assert!(matches!(err, UsageCollectorPluginError::Internal(_)));
}

#[test]
fn aggregate_response_rejects_an_unexpected_value_type() {
    let err = parse_aggregate_response(b"{\"agg\":[1,2]}\n", &[])
        .expect_err("an array aggregate value is not decodable as a decimal");
    assert!(matches!(err, UsageCollectorPluginError::Internal(_)));
}

/// Chunk boundaries that split a JSON line mid-object must still decode once
/// the newline arrives — this is the streaming path `aggregate` uses.
#[test]
fn aggregate_response_stream_parses_across_chunk_boundaries() {
    let dim_names = vec!["d0".to_owned()];
    let mut parser = AggregateNdjsonParser::new(dim_names);
    parser
        .push_chunk(br#"{"d0":"tenan"#)
        .expect("partial first chunk is not yet a line");
    parser
        .push_chunk(
            br#"t-a","agg":"1"}
{"d0":"ten"#,
        )
        .expect("first complete line + partial second");
    parser
        .push_chunk(
            br#"ant-b","agg":2}
"#,
        )
        .expect("second line completes");

    let buckets = parser.finish().expect("streamed NDJSON");
    assert_eq!(buckets.len(), 2);
    assert_eq!(buckets[0].key, vec!["tenant-a"]);
    assert_eq!(
        buckets[0].value.as_ref().map(ToString::to_string),
        Some("1".to_owned())
    );
    assert_eq!(buckets[1].key, vec!["tenant-b"]);
    assert_eq!(
        buckets[1].value.as_ref().map(ToString::to_string),
        Some("2".to_owned())
    );
}
