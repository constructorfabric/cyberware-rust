// Test modules using bare `panic!` opt in explicitly.
#![allow(clippy::panic)]

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use rust_decimal::Decimal;
use uuid::Uuid;

use usage_collector_sdk::{UsageCollectorPluginError, UsageRecord, UsageTypeGtsId};

use super::{
    AggregateNdjsonParser, ChRecordStore, InsertKind, catalog_lookup_sql, err_for_slot,
    insert_dedup_token, parse_aggregate_response, prefer_dedup_row, record_dedup_key,
    row_dedup_key, split_by_catalog,
};
use crate::domain::ports::RecordStore;
use crate::infra::metrics::Metrics;
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
// Both survive version resolution (distinct `id`s are distinct sort keys), so
// the lookup must choose between them deterministically and in favour of the
// caller's own record.

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

// ── err_for_slot ──────────────────────────────────────────────────────────────
//
// `UsageCollectorPluginError` is deliberately not `Clone`, so `create_batch`
// rebuilds an equivalent value per variant to place in every slot an error
// covers. A variant that loses its payload here would downgrade a
// caller-visible outcome (e.g. a retryable `Transient` becoming an opaque
// `Internal`), so each arm is pinned.

#[test]
fn err_for_slot_preserves_transient_payload() {
    let src = UsageCollectorPluginError::Transient {
        detail: "backend unreachable".to_owned(),
        retry_after_seconds: Some(7),
    };
    match err_for_slot(&src) {
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
fn err_for_slot_preserves_usage_type_not_found_gts_id() {
    let gts_id = UsageTypeGtsId::new(VCPU_GTS).unwrap();
    let src = UsageCollectorPluginError::UsageTypeNotFound {
        gts_id: gts_id.clone(),
    };
    match err_for_slot(&src) {
        UsageCollectorPluginError::UsageTypeNotFound { gts_id: got } => assert_eq!(got, gts_id),
        other => panic!("expected UsageTypeNotFound, got {other:?}"),
    }
}

#[test]
fn err_for_slot_preserves_internal_message() {
    let src = UsageCollectorPluginError::Internal("dedup lookup exploded".to_owned());
    match err_for_slot(&src) {
        UsageCollectorPluginError::Internal(msg) => assert_eq!(msg, "dedup lookup exploded"),
        other => panic!("expected Internal, got {other:?}"),
    }
}

/// The enum is `#[non_exhaustive]`, so an unmodelled variant must still degrade
/// to an `Internal` carrying the original text rather than being dropped.
#[test]
fn err_for_slot_falls_back_to_internal_for_other_variants() {
    let src = UsageCollectorPluginError::IdempotencyConflict {
        idempotency_key: "idem-1".to_owned(),
        existing_id: Uuid::from_u128(7),
    };
    let rendered = src.to_string();
    match err_for_slot(&src) {
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

/// The version-invariant half of a caller `$filter` is translated into the
/// inner scan and the `status` half above the resolution step, with the binds
/// landing in that same order.
///
/// Bind order is the sharp edge: `ClickHouse` `?` is positional, and the inner
/// `WHERE` precedes the outer one in the emitted text. Translating the outer
/// half first would still produce valid SQL — with the tenant UUID bound to the
/// status placeholder.
#[test]
fn push_split_filter_pushes_the_time_window_into_the_inner_scan() {
    use toolkit_odata::filter::{FilterField, FilterNode, FilterOp, ODataValue};
    use usage_collector_sdk::UsageRecordFilterField;

    use crate::infra::storage::query::translate::{SqlBind, SqlCtx};

    let field =
        |name: &str| UsageRecordFilterField::from_name(name).expect("field is on the schema");
    let tenant = Uuid::from_u128(7);
    let from =
        chrono::DateTime::from_timestamp_micros(1_767_225_600_000_000).expect("timestamp in range");

    // The shape the gateway sends: tenant + `[from, …)` window + status.
    let node = FilterNode::and(vec![
        FilterNode::binary(field("tenant_id"), FilterOp::Eq, ODataValue::Uuid(tenant)),
        FilterNode::binary(
            field("created_at"),
            FilterOp::Ge,
            ODataValue::DateTime(from),
        ),
        FilterNode::binary(
            field("status"),
            FilterOp::Eq,
            ODataValue::String("active".to_owned()),
        ),
    ]);

    // The metadata side-channel already occupies the inner context, exactly as
    // it does at the call sites.
    let mut inner_ctx = SqlCtx::new();
    let mut inner_clauses = vec!["gts_id = ?".to_owned()];
    let mut outer_ctx = SqlCtx::new();
    let mut outer_clauses = vec!["status = 'active'".to_owned()];

    ChRecordStore::push_split_filter(
        &node,
        &mut inner_ctx,
        &mut inner_clauses,
        &mut outer_ctx,
        &mut outer_clauses,
    )
    .expect("filter translates");

    assert_eq!(
        inner_clauses,
        vec![
            "gts_id = ?".to_owned(),
            "tenant_id = ?".to_owned(),
            "created_at >= fromUnixTimestamp64Micro(?)".to_owned(),
        ],
        "tenant_id and created_at are key-prefix columns and must prune the scan"
    );
    assert_eq!(
        outer_clauses,
        vec!["status = 'active'".to_owned(), "status = ?".to_owned()],
        "status must stay above the version-resolution step"
    );

    assert!(matches!(&inner_ctx.binds[0], SqlBind::Uuid(u) if *u == tenant));
    assert!(matches!(
        &inner_ctx.binds[1],
        SqlBind::DateTime64Micros(1_767_225_600_000_000)
    ));
    assert_eq!(inner_ctx.binds.len(), 2);
    assert_eq!(outer_ctx.binds.len(), 1);
    assert!(matches!(&outer_ctx.binds[0], SqlBind::Str(s) if s == "active"));
}

// ── Offline store ─────────────────────────────────────────────────────────────

/// Build a store over an offline client.
///
/// Port 1 is reserved and never bound, so any query that is actually issued
/// fails fast (connection refused) instead of blocking. The `clickhouse` crate's
/// default address (`http://localhost:8123`) would let a real local server
/// answer these "offline" tests.
fn offline_store() -> ChRecordStore {
    ChRecordStore::new(
        clickhouse::Client::default().with_url("http://127.0.0.1:1"),
        Arc::new(Metrics::new()),
        // Generous: every assertion here either short-circuits before I/O or
        // fails fast on connection refused, so the deadline is never what a
        // test observes.
        std::time::Duration::from_secs(30),
        // Matches the production default, so the offline tier exercises the
        // shipped configuration.
        true,
    )
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

/// [`make_record`] under a caller-chosen usage type, for multi-`gts_id` batches.
fn make_record_for(gts_id: &str, id: Uuid, tenant_id: Uuid, created_at_micros: i64) -> UsageRecord {
    UsageRecord {
        gts_id: UsageTypeGtsId::new(gts_id).unwrap(),
        ..make_record(id, tenant_id, created_at_micros)
    }
}

// ── Empty-input short circuits ────────────────────────────────────────────────

/// An all-absorbed / all-rejected batch leaves nothing to write; the insert must
/// then be skipped rather than sending an empty INSERT to `ClickHouse`.
#[tokio::test]
async fn insert_records_with_no_rows_is_a_no_op() {
    offline_store()
        .insert_records(&[], std::time::Instant::now(), InsertKind::Record)
        .await
        .expect("an empty row set must not touch the backend");
}

// ── insert_dedup_token ────────────────────────────────────────────────────────

/// Two statements writing the same rows must dedup against each other whatever
/// order the rows arrive in — a retried batch is not guaranteed to be composed
/// in the same order — and a row repeated inside one statement must not change
/// the token either.
#[test]
fn insert_dedup_token_is_order_insensitive_and_ignores_repeated_rows() {
    let tenant = Uuid::from_u128(2);
    let a = make_row(Uuid::from_u128(10), tenant, 1_700_000_000_000_000, 1);
    let b = make_row(Uuid::from_u128(11), tenant, 1_700_000_000_000_001, 1);
    let c = make_row(Uuid::from_u128(12), tenant, 1_700_000_000_000_002, 1);

    let forward = insert_dedup_token(&[a.clone(), b.clone(), c.clone()], InsertKind::Record);
    let reversed = insert_dedup_token(&[c.clone(), b.clone(), a.clone()], InsertKind::Record);
    let repeated = insert_dedup_token(&[a.clone(), a.clone(), b.clone(), c], InsertKind::Record);
    assert_eq!(forward, reversed, "row order must not enter the token");
    assert_eq!(forward, repeated, "a repeated row must not enter the token");

    let other = insert_dedup_token(&[a, b], InsertKind::Record);
    assert_ne!(
        forward, other,
        "a different row set must produce a different token"
    );
}

/// A single create and a batch containing only that record are the same write
/// and must dedup against each other at the engine.
#[test]
fn insert_dedup_token_single_row_equals_batch_of_one() {
    let row = make_row(
        Uuid::from_u128(10),
        Uuid::from_u128(2),
        1_700_000_000_000_000,
        1,
    );
    assert_eq!(
        insert_dedup_token(std::slice::from_ref(&row), InsertKind::Record),
        insert_dedup_token(&[row], InsertKind::Record),
    );
}

/// A deactivation marker shares its source row's `id`, so without the kind
/// discriminator a cascade issued inside the dedup window of the create that
/// wrote those ids would be dropped as a retry of it.
#[test]
fn insert_dedup_token_discriminates_markers_from_records() {
    let row = make_row(
        Uuid::from_u128(10),
        Uuid::from_u128(2),
        1_700_000_000_000_000,
        1,
    );
    let mut marker = row.clone();
    marker.status = UsageRecordStatusCode::Inactive;
    marker.version = row.version + 1;
    assert_ne!(
        insert_dedup_token(std::slice::from_ref(&row), InsertKind::Record),
        insert_dedup_token(std::slice::from_ref(&marker), InsertKind::Marker),
        "the same id under the two kinds must not share a token"
    );
    // Only `id` and `kind` enter the token: the payload, `status` and
    // `version` do not, so a retried marker set with fresh versions still
    // matches the one already written.
    let mut retried = marker.clone();
    retried.version += 5;
    assert_eq!(
        insert_dedup_token(std::slice::from_ref(&marker), InsertKind::Marker),
        insert_dedup_token(std::slice::from_ref(&retried), InsertKind::Marker),
    );
}

/// The token is a setting value on the wire; `ClickHouse` accepts any string,
/// but a UUID keeps it fixed-width and free of characters that would need
/// quoting in `system.query_log` queries.
#[test]
fn insert_dedup_token_is_a_uuid_string() {
    let row = make_row(
        Uuid::from_u128(10),
        Uuid::from_u128(2),
        1_700_000_000_000_000,
        1,
    );
    let token = insert_dedup_token(&[row], InsertKind::Record);
    assert!(
        Uuid::parse_str(&token).is_ok(),
        "token must be a UUID: {token}"
    );
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

// ── Batch pipeline (no I/O) ───────────────────────────────────────────────────
//
// `create_batch` is three statements around pure composition. The pure parts
// are exercised here directly; the statements themselves are covered by the
// feature-gated live suite.

const RAM_GTS: &str = "gts.cf.core.uc.usage_record.v1~cf.compute._.ram_gb.v1";

/// One bound parameter per distinct usage type, in a deterministic order, and
/// no caller-supplied identifier in the SQL text itself.
#[test]
fn catalog_lookup_sql_binds_one_str_per_distinct_gts_id() {
    use std::collections::BTreeSet;

    use crate::infra::storage::query::translate::SqlBind;

    let gts_ids: BTreeSet<&str> = [VCPU_GTS, RAM_GTS, VCPU_GTS].into_iter().collect();
    let (sql, ctx) = catalog_lookup_sql(&gts_ids);

    assert_eq!(
        sql,
        "SELECT gts_id FROM usage_type_catalog WHERE gts_id IN (?, ?)"
    );
    assert_eq!(ctx.binds.len(), 2, "one bind per distinct gts_id");
    // `BTreeSet` order: RAM_GTS sorts before VCPU_GTS.
    assert!(matches!(&ctx.binds[0], SqlBind::Str(s) if s == RAM_GTS));
    assert!(matches!(&ctx.binds[1], SqlBind::Str(s) if s == VCPU_GTS));
    assert!(
        !sql.contains(VCPU_GTS),
        "identifiers are bound, never inlined into the SQL text"
    );
}

/// A missing usage type marks exactly the slots that reference it, at their
/// own input positions, and hands every other record on to the dedup step.
#[test]
fn catalog_miss_marks_only_that_gts_ids_slots() {
    let tenant_id = Uuid::from_u128(2);
    let records = vec![
        make_record_for(
            RAM_GTS,
            Uuid::from_u128(1),
            tenant_id,
            1_700_000_000_000_000,
        ),
        make_record_for(
            VCPU_GTS,
            Uuid::from_u128(3),
            tenant_id,
            1_700_000_000_000_001,
        ),
        make_record_for(
            RAM_GTS,
            Uuid::from_u128(4),
            tenant_id,
            1_700_000_000_000_002,
        ),
    ];
    let known: HashSet<String> = [VCPU_GTS.to_owned()].into_iter().collect();
    let mut outcomes = vec![None, None, None];

    let passed = split_by_catalog(&records, &known, &mut outcomes);

    assert_eq!(passed, vec![1], "only the registered type's record passes");
    for idx in [0, 2] {
        match &outcomes[idx] {
            Some(Err(UsageCollectorPluginError::UsageTypeNotFound { gts_id })) => {
                assert_eq!(gts_id.as_ref(), RAM_GTS, "slot {idx} names its own gts_id");
            }
            other => panic!("slot {idx} must be UsageTypeNotFound, got {other:?}"),
        }
    }
    assert!(
        outcomes[1].is_none(),
        "a passed slot is left for the dedup step to decide"
    );
}

/// Composed rows get contiguous versions from the batch base; a record absorbed
/// from storage composes no row and consumes no version; an identical in-batch
/// duplicate shares its twin's row and slot list.
#[test]
fn compose_batch_assigns_contiguous_versions_and_skips_absorbed_rows() {
    let tenant_id = Uuid::from_u128(8);
    let base_micros = 1_700_000_000_000_000_i64;
    let stored_id = Uuid::from_u128(7);
    let records = vec![
        make_record(Uuid::from_u128(1), tenant_id, base_micros + 1),
        // Already stored: absorbed, composes nothing.
        make_record(stored_id, tenant_id, base_micros),
        make_record(Uuid::from_u128(3), tenant_id, base_micros + 3),
        // Identical in-batch twin of the row above.
        make_record(Uuid::from_u128(3), tenant_id, base_micros + 3),
    ];
    let stored = make_row(stored_id, tenant_id, base_micros, 1);
    let existing: HashMap<_, _> = [(row_dedup_key(&stored), stored)].into_iter().collect();
    let passed = vec![0, 1, 2, 3];
    let mut outcomes = vec![None, None, None, None];

    let (to_insert, row_slots) =
        offline_store().compose_batch(&records, &passed, &existing, 1_000, &mut outcomes);

    let versions: Vec<u64> = to_insert.iter().map(|r| r.version).collect();
    assert_eq!(
        versions,
        vec![1_000, 1_001],
        "one version per composed row, contiguous from the base"
    );
    assert_eq!(
        row_slots,
        vec![vec![0], vec![2, 3]],
        "the twin hangs off its sibling's row rather than composing its own"
    );
    match &outcomes[1] {
        Some(Ok(absorbed)) => assert_eq!(absorbed.id, stored_id, "absorbed from storage"),
        other => panic!("slot 1 must be absorbed, got {other:?}"),
    }
    for idx in [0, 2, 3] {
        assert!(
            matches!(outcomes[idx], Some(Ok(_))),
            "slot {idx} must be Ok, got {:?}",
            outcomes[idx]
        );
    }
}

/// A backend failure on the batch's first read is still a per-record outcome,
/// never a batch-level `Err` that discards the whole submission.
///
/// The failure here comes from the unreachable server, so its classification
/// is whatever the client reports for a refused connection; the contract under
/// test is the per-record shape of the result, not the variant.
#[tokio::test]
async fn create_batch_reports_backend_failures_per_record() {
    let store = offline_store();
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

// ── build_aggregate_sql / build_list_sql ──────────────────────────────────────

/// The realistic scan clause set: `gts_id` is always pinned, and non-`SUM` ops
/// add the `corrects_id` partition.
fn inner() -> Vec<String> {
    vec!["gts_id = ?".to_owned(), "corrects_id IS NULL".to_owned()]
}

/// The exact survivor predicate the aggregate must emit for [`inner`]: raw
/// active rows whose id carries no marker, with the scan text repeated inside
/// the marker subquery so it prunes on the same key range.
const AGG_SURVIVORS: &str = "status = 'active' AND id NOT IN \
     (SELECT id FROM usage_records WHERE gts_id = ? AND corrects_id IS NULL \
     AND status = 'inactive')";

/// The aggregate has no version-resolving level: no `GROUP BY` on the sort key,
/// no `HAVING`, no `LIMIT 1 BY`, no `argMax`, and no nested `SELECT` other than
/// the marker subquery. Deactivation is handled by the survivor predicate.
#[test]
fn aggregate_sql_is_a_single_level_scan_with_the_marker_anti_join() {
    let sql = ChRecordStore::build_aggregate_sql("SUM(value) AS agg", &inner(), &[], "", "");
    assert_eq!(
        sql,
        format!(
            "SELECT SUM(value) AS agg FROM usage_records \
             WHERE gts_id = ? AND corrects_id IS NULL AND {AGG_SURVIVORS}"
        )
    );
    for forbidden in [
        "HAVING",
        "LIMIT 1 BY",
        "argMax",
        "GROUP BY gts_id",
        "version",
    ] {
        assert!(
            !sql.contains(forbidden),
            "no resolution construct may remain (`{forbidden}`): {sql}"
        );
    }
    assert_eq!(
        sql.matches("SELECT").count(),
        2,
        "the marker subquery is the only nested SELECT: {sql}"
    );
}

/// Every scan predicate is repeated verbatim inside the marker subquery, so the
/// caller binds the scan values twice — the placeholder count pins that
/// contract, and a scan clause with its own `?` must be doubled too.
#[test]
fn aggregate_sql_repeats_every_scan_predicate_inside_the_marker_subquery() {
    let mut scan = inner();
    scan.push("created_at >= fromUnixTimestamp64Micro(?)".to_owned());
    scan.push("metadata[?] IN (?, ?)".to_owned());
    let scan_placeholders: usize = scan.iter().map(|c| c.matches('?').count()).sum();
    let sql = ChRecordStore::build_aggregate_sql("SUM(value) AS agg", &scan, &[], "", "");
    assert_eq!(
        sql.matches('?').count(),
        2 * scan_placeholders,
        "scan binds are applied twice, once per copy of the scan text: {sql}"
    );
    let scan_text = scan.join(" AND ");
    assert_eq!(
        sql.matches(scan_text.as_str()).count(),
        2,
        "the scan text must appear as the scan predicate and inside the subquery: {sql}"
    );
}

/// The `status`-naming half of `$filter` trails the survivor predicate in the
/// same `WHERE`, so its binds come after both copies of the scan binds.
#[test]
fn aggregate_sql_appends_version_dependent_filters_after_the_survivor_predicate() {
    let sql = ChRecordStore::build_aggregate_sql(
        "SUM(value) AS agg",
        &inner(),
        &["status = ?".to_owned()],
        " GROUP BY 1",
        " LIMIT 100001",
    );
    assert!(
        sql.contains(&format!(
            "{AGG_SURVIVORS} AND status = ? GROUP BY 1 LIMIT 100001"
        )),
        "the trailing conjunct must follow the survivor predicate: {sql}"
    );
    assert_eq!(
        sql.matches("WHERE").count(),
        2,
        "one WHERE for the query, one inside the marker subquery: {sql}"
    );
}

/// The syntax-error guard stated as a property rather than as literal strings,
/// across every combination of the branches that vary.
#[test]
fn aggregate_sql_never_emits_an_empty_clause_or_a_double_space() {
    let outer_variants: [Vec<String>; 2] = [vec![], vec!["status = ?".to_owned()]];
    let grouping_variants = [("", ""), (" GROUP BY 1", " LIMIT 100001")];

    for outer in &outer_variants {
        for (group_by, limit_clause) in grouping_variants {
            let sql = ChRecordStore::build_aggregate_sql(
                "SUM(value) AS agg",
                &inner(),
                outer,
                group_by,
                limit_clause,
            );
            let ctx = format!("outer={outer:?} group_by={group_by:?} sql={sql}");
            assert!(!sql.contains("  "), "double space: {ctx}");
            assert!(!sql.contains("WHERE )"), "empty WHERE before close: {ctx}");
            assert!(!sql.contains("WHERE GROUP BY"), "empty WHERE: {ctx}");
            assert!(!sql.contains("WHERE LIMIT"), "empty WHERE: {ctx}");
            assert!(!sql.contains("AND AND"), "empty conjunct: {ctx}");
            assert!(!sql.contains("AND GROUP BY"), "dangling AND: {ctx}");
            assert!(!sql.ends_with("WHERE"), "trailing WHERE: {ctx}");
            assert!(!sql.ends_with("AND"), "trailing AND: {ctx}");
        }
    }
}

/// The ungrouped shape (`dim_count == 0`) appends nothing after the survivor
/// predicate. The gateway depends on this call producing exactly one bucket, so
/// the text must not gain a stray clause that could filter it away.
#[test]
fn aggregate_sql_ungrouped_ends_at_the_survivor_predicate() {
    let sql = ChRecordStore::build_aggregate_sql("SUM(value) AS agg", &inner(), &[], "", "");
    assert!(
        sql.ends_with("AND status = 'inactive')"),
        "no grouping and no trailing filter means nothing follows: {sql}"
    );
}

/// The scan keeps every version-invariant predicate, where it prunes.
/// `tenant_id` and `created_at` reach `aggregate` only through `$filter`, and
/// with `gts_id` they are the whole sort-key prefix.
#[test]
fn aggregate_sql_keeps_invariant_predicates_in_the_scan() {
    let mut scan = inner();
    scan.push("created_at >= fromUnixTimestamp64Micro(?)".to_owned());
    let sql = ChRecordStore::build_aggregate_sql("SUM(value) AS agg", &scan, &[], "", "");
    assert!(
        sql.starts_with(
            "SELECT SUM(value) AS agg FROM usage_records WHERE gts_id = ? AND corrects_id IS NULL \
             AND created_at >= fromUnixTimestamp64Micro(?) AND status = 'active'"
        ),
        "invariant conjuncts belong on the scan, ahead of the survivor predicate: {sql}"
    );
}

/// `list` is likewise single-level: no version sort, no `LIMIT 1 BY`, the
/// caller's `ORDER BY` and look-ahead `LIMIT` applied once, after every
/// predicate. Its survivor predicate keeps markers (they *are* the resolved
/// inactive rows) and drops only the active rows a marker supersedes.
#[test]
fn list_sql_is_a_single_level_scan_with_the_resolved_survivors() {
    let scan = vec!["gts_id = ?".to_owned()];
    let sql = ChRecordStore::build_list_sql(&scan, &[], "created_at ASC, id ASC", 101);
    assert_eq!(
        sql,
        format!(
            "SELECT {} FROM usage_records WHERE gts_id = ? AND (status = 'inactive' OR id NOT IN \
             (SELECT id FROM usage_records WHERE gts_id = ? AND status = 'inactive')) \
             ORDER BY created_at ASC, id ASC LIMIT 101",
            super::RECORD_COLUMNS
        )
    );
    for forbidden in ["LIMIT 1 BY", "version DESC", "argMax", "GROUP BY"] {
        assert!(
            !sql.contains(forbidden),
            "no resolution construct may remain (`{forbidden}`): {sql}"
        );
    }
}

/// Bind contract for `list`: scan binds twice, then the trailing binds (the
/// `status` half of `$filter`, then the keyset tuple) in that order.
#[test]
fn list_sql_places_trailing_predicates_after_the_survivors_and_before_order_by() {
    let scan = vec![
        "gts_id = ?".to_owned(),
        "tenant_id = ?".to_owned(),
        "metadata[?] IN (?)".to_owned(),
    ];
    let outer = vec![
        "status = ?".to_owned(),
        "(created_at, id) > (fromUnixTimestamp64Micro(?), ?)".to_owned(),
    ];
    let sql = ChRecordStore::build_list_sql(&scan, &outer, "created_at ASC, id ASC", 51);
    let scan_placeholders: usize = scan.iter().map(|c| c.matches('?').count()).sum();
    let outer_placeholders: usize = outer.iter().map(|c| c.matches('?').count()).sum();
    assert_eq!(
        sql.matches('?').count(),
        2 * scan_placeholders + outer_placeholders,
        "scan binds twice, trailing binds once: {sql}"
    );
    assert!(
        sql.contains(
            "AND status = 'inactive')) AND status = ? \
             AND (created_at, id) > (fromUnixTimestamp64Micro(?), ?) \
             ORDER BY created_at ASC, id ASC LIMIT 51"
        ),
        "status half, then keyset, then ORDER BY / LIMIT: {sql}"
    );
    let subquery_start = sql.find("(SELECT id").expect("marker subquery present");
    let keyset_start = sql.find("(created_at, id) >").expect("keyset present");
    assert!(
        keyset_start > subquery_start,
        "the keyset predicate must follow the survivor predicate: {sql}"
    );
}
