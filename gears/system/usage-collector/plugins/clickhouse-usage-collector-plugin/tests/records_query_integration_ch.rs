#![cfg(feature = "clickhouse")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
//! `ClickHouse`-backed integration tests for [`ChRecordStore`] keyset
//! pagination and pushed-down aggregation:
//! - keyset pagination (first page + cursor follow, no overlap/gap),
//! - `$filter` by `tenant_id`,
//! - metadata side-channel filtering,
//! - SUM nets compensation, COUNT active-only, GROUP BY `resource_id`,
//! - `MAX_AGGREGATION_BUCKETS + 1` cap enforcement.
//!
//! All reads use `FINAL` (enforced by the store); tests insert with distinct
//! `created_at` values so the `(created_at, id)` order is fully observable.
//! Requires Docker.

mod common;

use std::collections::BTreeMap;

use bigdecimal::BigDecimal;
use rust_decimal::Decimal;
use time::OffsetDateTime;
use uuid::Uuid;

use toolkit_odata::ast::{CompareOperator, Expr, Value};
use toolkit_odata::{CursorV1, ODataOrderBy, ODataQuery, OrderKey, SortDir};

use usage_collector_sdk::{
    AggregationDimension, AggregationOp, AggregationSpec, MAX_AGGREGATION_BUCKETS, MetadataFilter,
    MetadataKey, UsageRecord,
};

use clickhouse_usage_collector_plugin::domain::ports::{CatalogStore, RecordStore};

const VCPU_GTS: &str = "gts.cf.core.uc.usage_record.v1~cf.compute._.vcpu_hours.v1";

/// Base instant so each record's `created_at = base + i` is distinct.
///
/// Taken from [`common::fixture_base_ts`] rather than a hardcoded epoch so the
/// rows stay inside the `usage_records` TTL window (see that function).
fn base_ts() -> i64 {
    common::fixture_base_ts()
}

/// Bring up containers and register `VCPU_GTS`.
///
/// Returns `None` when Docker is unavailable, so the caller can skip its own
/// test body with an early `return` instead of terminating the whole test
/// binary (see [`common::bring_up_or_skip`]).
async fn setup_with_type(
    gts: &str,
    fields: &[&str],
) -> Option<(common::ChHarness, impl RecordStore + Clone)> {
    let h = common::bring_up_or_skip().await?;
    let catalog = common::catalog_store(&h);
    catalog
        .create(common::fixture_usage_type(gts, "counter", fields))
        .await
        .expect("register usage type for referential integrity");
    let store = common::record_store(&h);
    Some((h, store))
}

fn record_at(gts: &str, tenant: Uuid, seq: u128, i: i64) -> UsageRecord {
    let mut rec = common::fixture_usage_record(
        gts,
        tenant,
        &format!("idem-{seq}"),
        Decimal::new(i + 1, 0),
        seq,
    );
    rec.created_at =
        OffsetDateTime::from_unix_timestamp(base_ts() + i).expect("valid created_at instant");
    rec
}

fn created_at_id_asc() -> ODataOrderBy {
    ODataOrderBy(vec![
        OrderKey {
            field: "created_at".to_owned(),
            dir: SortDir::Asc,
        },
        OrderKey {
            field: "id".to_owned(),
            dir: SortDir::Asc,
        },
    ])
}

/// Keyset pagination: first page respects `$top` and yields a `next_cursor`.
/// Following the cursor yields remaining records with no overlap or gap.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_list_keyset_first_page_and_cursor_follow() {
    let Some((_h, store)) = setup_with_type(VCPU_GTS, &[]).await else {
        return;
    };
    let tenant = Uuid::from_u128(0x2001);

    let mut expected: Vec<Uuid> = Vec::new();
    for i in 0..5 {
        let seq = 0x2001_0000 + u128::try_from(i).unwrap();
        let rec = record_at(VCPU_GTS, tenant, seq, i);
        expected.push(rec.id);
        store.create(rec).await.expect("create record");
    }

    let order = created_at_id_asc();
    let mut seen: Vec<Uuid> = Vec::new();
    let mut cursor: Option<CursorV1> = None;

    loop {
        let mut query = ODataQuery::new().with_limit(2).with_order(order.clone());
        if let Some(c) = cursor.take() {
            query = query.with_cursor(c);
        }
        let page = store
            .list(common::fixture_gts_id(VCPU_GTS), &query, &[])
            .await
            .expect("list page");

        for item in &page.items {
            assert!(
                !seen.contains(&item.id),
                "no record appears on two pages (overlap)"
            );
            seen.push(item.id);
        }

        match page.page_info.next_cursor {
            Some(token) => {
                cursor = Some(CursorV1::decode(&token).expect("decode next cursor"));
            }
            None => break,
        }
    }

    let mut seen_sorted = seen.clone();
    seen_sorted.sort();
    let mut expected_sorted = expected.clone();
    expected_sorted.sort();
    assert_eq!(
        seen_sorted, expected_sorted,
        "walking all pages yields every record exactly once (no gap, no overlap)"
    );
    assert_eq!(seen.len(), 5, "exactly the five inserted records");
}

/// `$filter` by `tenant_id` narrows list results to the specified tenant.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_list_keyset_respects_filter() {
    let Some((_h, store)) = setup_with_type(VCPU_GTS, &[]).await else {
        return;
    };
    let tenant_a = Uuid::from_u128(0x2002_000A);
    let tenant_b = Uuid::from_u128(0x2002_000B);

    store
        .create(record_at(VCPU_GTS, tenant_a, 0x2002_0001, 0))
        .await
        .expect("create A1");
    store
        .create(record_at(VCPU_GTS, tenant_a, 0x2002_0002, 1))
        .await
        .expect("create A2");
    store
        .create(record_at(VCPU_GTS, tenant_b, 0x2002_0003, 2))
        .await
        .expect("create B1");

    let filter = Expr::Compare(
        Box::new(Expr::Identifier("tenant_id".to_owned())),
        CompareOperator::Eq,
        Box::new(Expr::Value(Value::Uuid(tenant_a))),
    );
    let query = ODataQuery::new()
        .with_order(created_at_id_asc())
        .with_filter(filter);

    let page = store
        .list(common::fixture_gts_id(VCPU_GTS), &query, &[])
        .await
        .expect("list filtered by tenant");

    assert_eq!(page.items.len(), 2, "only tenant A's two records match");
    for item in &page.items {
        assert_eq!(
            item.tenant_id, tenant_a,
            "every returned record is tenant A"
        );
    }
}

/// Metadata side-channel filter narrows results to the matching `region` value.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_list_metadata_filter_excludes_non_matching_rows() {
    let Some((_h, store)) = setup_with_type(VCPU_GTS, &["region"]).await else {
        return;
    };
    let tenant = Uuid::from_u128(0x2003);

    let regions = ["us-east-1", "us-east-1", "eu-west-1"];
    for (i, region) in regions.iter().enumerate() {
        let idx = i64::try_from(i).unwrap();
        let seq = 0x2003_0000 + u128::try_from(i).unwrap();
        let mut rec = record_at(VCPU_GTS, tenant, seq, idx);
        let mut meta = BTreeMap::new();
        meta.insert(
            MetadataKey::new("region").expect("valid metadata key"),
            (*region).to_owned(),
        );
        rec.metadata = meta;
        store.create(rec).await.expect("create record");
    }

    let filter = MetadataFilter::new("region", ["us-east-1"]).expect("valid metadata filter");
    let query = ODataQuery::new().with_order(created_at_id_asc());

    let page = store
        .list(
            common::fixture_gts_id(VCPU_GTS),
            &query,
            std::slice::from_ref(&filter),
        )
        .await
        .expect("list with metadata filter");

    assert_eq!(
        page.items.len(),
        2,
        "only the two us-east-1 records match the metadata filter"
    );
    for item in &page.items {
        assert_eq!(
            item.metadata
                .get(&MetadataKey::new("region").unwrap())
                .map(String::as_str),
            Some("us-east-1"),
            "every returned record carries the filtered metadata value"
        );
    }
}

/// SUM aggregation nets the compensation row: `10 + (-3) = 7`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_aggregate_sum_nets_compensation() {
    let Some((_h, store)) = setup_with_type(VCPU_GTS, &[]).await else {
        return;
    };
    let tenant = Uuid::from_u128(0x3001);

    let mut original = record_at(VCPU_GTS, tenant, 0x3001_0001, 0);
    original.value = Decimal::new(10, 0);
    let original_id = original.id;
    store.create(original).await.expect("create original");

    let mut compensation = record_at(VCPU_GTS, tenant, 0x3001_0002, 1);
    compensation.value = Decimal::new(-3, 0);
    compensation.corrects_id = Some(original_id);
    store
        .create(compensation)
        .await
        .expect("create compensation");

    let spec = AggregationSpec {
        op: AggregationOp::Sum,
        group_by: Vec::new(),
    };
    let result = store
        .aggregate(
            common::fixture_gts_id(VCPU_GTS),
            &ODataQuery::new(),
            &[],
            spec,
        )
        .await
        .expect("aggregate sum");

    assert_eq!(
        result.buckets.len(),
        1,
        "empty group_by yields exactly one bucket"
    );
    let bucket = &result.buckets[0];
    assert!(bucket.key.is_empty(), "no grouping -> empty bucket key");
    assert_eq!(
        bucket.value,
        Some(BigDecimal::from(7_i64)),
        "SUM nets the active compensation: 10 + (-3) = 7"
    );
}

/// COUNT excludes inactive (deactivated) rows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_aggregate_count_active_only() {
    let Some((_h, store)) = setup_with_type(VCPU_GTS, &[]).await else {
        return;
    };
    let tenant = Uuid::from_u128(0x3002);

    // Insert three active rows, then deactivate one.
    for i in 0..3 {
        let seq = 0x3002_0000 + u128::try_from(i).unwrap();
        store
            .create(record_at(VCPU_GTS, tenant, seq, i))
            .await
            .expect("create record");
    }
    let first_id = Uuid::from_u128(0x3002_0000);
    store.deactivate(first_id).await.expect("deactivate first");
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let spec = AggregationSpec {
        op: AggregationOp::Count,
        group_by: Vec::new(),
    };
    let result = store
        .aggregate(
            common::fixture_gts_id(VCPU_GTS),
            &ODataQuery::new(),
            &[],
            spec,
        )
        .await
        .expect("aggregate count active only");

    assert_eq!(result.buckets.len(), 1, "empty group_by -> one bucket");
    assert_eq!(
        result.buckets[0].value,
        Some(BigDecimal::from(2_i64)),
        "COUNT excludes the deactivated row; two active rows remain"
    );
}

/// GROUP BY `resource_id` aggregation yields one bucket per distinct resource.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_aggregate_group_by_resource_id() {
    let Some((_h, store)) = setup_with_type(VCPU_GTS, &[]).await else {
        return;
    };
    let tenant = Uuid::from_u128(0x3003);

    let rows = [
        ("idem-3003-1", 4_i64, 0x3003_0001_u128, "res-a", 0_i64),
        ("idem-3003-2", 6, 0x3003_0002, "res-a", 1),
        ("idem-3003-3", 5, 0x3003_0003, "res-b", 2),
    ];
    for (idem, value, seq, resource_id, ts) in rows {
        let mut rec = common::fixture_usage_record_with_resource(
            VCPU_GTS,
            tenant,
            idem,
            Decimal::new(value, 0),
            seq,
            resource_id,
        );
        rec.created_at = OffsetDateTime::from_unix_timestamp(base_ts() + ts).unwrap();
        store.create(rec).await.expect("create record");
    }

    let spec = AggregationSpec {
        op: AggregationOp::Sum,
        group_by: vec![AggregationDimension::ResourceId],
    };
    let result = store
        .aggregate(
            common::fixture_gts_id(VCPU_GTS),
            &ODataQuery::new(),
            &[],
            spec,
        )
        .await
        .expect("aggregate group by resource_id");

    assert_eq!(
        result.buckets.len(),
        2,
        "one bucket per distinct resource_id"
    );
    let mut got: Vec<(String, Option<BigDecimal>)> = result
        .buckets
        .iter()
        .map(|b| {
            assert_eq!(b.key.len(), 1, "single grouped dimension -> one key entry");
            (b.key[0].clone(), b.value.clone())
        })
        .collect();
    got.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        got,
        vec![
            ("res-a".to_owned(), Some(BigDecimal::from(10_i64))),
            ("res-b".to_owned(), Some(BigDecimal::from(5_i64))),
        ],
        "each resource_id bucket carries its summed value"
    );
}

/// Inserting `MAX_AGGREGATION_BUCKETS + 2` distinct groups causes the store to
/// return exactly `MAX_AGGREGATION_BUCKETS + 1` buckets — the gateway's
/// over-limit sentinel row. The call does not materialize an unbounded set.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_aggregate_cap_at_max_aggregation_buckets() {
    let Some((_h, store)) = setup_with_type(VCPU_GTS, &[]).await else {
        return;
    };
    let tenant = Uuid::from_u128(0x3010);

    // Insert MAX_AGGREGATION_BUCKETS + 2 records with distinct resource_ids so
    // each maps to its own bucket. Use distinct (created_at, id) 4-tuples.
    let n = MAX_AGGREGATION_BUCKETS + 2;
    let mut batch: Vec<_> = (0..n)
        .map(|i| {
            let seq = 0x3010_0000 + u128::try_from(i).unwrap();
            let ts = i64::try_from(i).unwrap();
            let resource_id = format!("res-{i}");
            let idem = format!("idem-cap-{i}");
            let mut rec = common::fixture_usage_record_with_resource(
                VCPU_GTS,
                tenant,
                &idem,
                Decimal::ONE,
                seq,
                &resource_id,
            );
            rec.created_at = OffsetDateTime::from_unix_timestamp(base_ts() + ts).expect("valid ts");
            rec
        })
        .collect();

    // Insert in chunks to avoid excessively large batch (10k rows at once is fine for CH).
    for chunk in batch.chunks(1000) {
        store
            .create_batch(chunk.to_vec())
            .await
            .expect("batch insert chunk");
    }
    // Drain the batch vec here (already consumed above by to_vec).
    batch.clear();

    let spec = AggregationSpec {
        op: AggregationOp::Sum,
        group_by: vec![AggregationDimension::ResourceId],
    };
    let result = store
        .aggregate(
            common::fixture_gts_id(VCPU_GTS),
            &ODataQuery::new(),
            &[],
            spec,
        )
        .await
        .expect("aggregate with cap");

    assert_eq!(
        result.buckets.len(),
        MAX_AGGREGATION_BUCKETS + 1,
        "store returns exactly MAX_AGGREGATION_BUCKETS + 1 rows \u{2014} the over-limit sentinel"
    );
}

/// Paginating on a nullable sort key fails loudly when the boundary row's key
/// is `NULL`, instead of minting a cursor that cannot address that row.
///
/// `subject_id` is orderable and keyset-eligible, but a `NULL` has no cursor
/// key. Encoding the page boundary as an empty string would make the follow-up
/// page skip or repeat rows, so the store refuses to build the cursor.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_list_refuses_a_cursor_when_the_boundary_sort_key_is_null() {
    use usage_collector_sdk::UsageCollectorPluginError;

    let Some((_h, store)) = setup_with_type(VCPU_GTS, &[]).await else {
        return;
    };
    let tenant = Uuid::from_u128(0x2004);

    // The fixture leaves `subject_ref` unset, so every row has subject_id NULL.
    for i in 0..3 {
        let seq = 0x2004_0000 + u128::try_from(i).unwrap();
        store
            .create(record_at(VCPU_GTS, tenant, seq, i))
            .await
            .expect("create record");
    }

    let query = ODataQuery::new()
        .with_limit(2)
        .with_order(ODataOrderBy(vec![OrderKey {
            field: "subject_id".to_owned(),
            dir: SortDir::Asc,
        }]));

    let err = store
        .list(common::fixture_gts_id(VCPU_GTS), &query, &[])
        .await
        .expect_err("a NULL boundary sort key must not yield a cursor");
    match err {
        UsageCollectorPluginError::Internal(msg) => assert!(
            msg.contains("subject_id") && msg.contains("no cursor key"),
            "unexpected message: {msg}"
        ),
        other => panic!("expected Internal, got {other:?}"),
    }
}

// ── Filter validation ────────────────────────────────────────────────────────

/// A `$filter` naming a column outside the record allowlist is rejected by both
/// query paths. Ignoring it instead would widen the result set (or the summed
/// set) beyond what the caller asked for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn ch_list_and_aggregate_reject_a_non_allowlisted_filter_field() {
    use usage_collector_sdk::UsageCollectorPluginError;

    let Some((_h, store)) = setup_with_type(VCPU_GTS, &[]).await else {
        return;
    };

    // `version` is a physical column but not an SPI-filterable field.
    let filter = Expr::Compare(
        Box::new(Expr::Identifier("version".to_owned())),
        CompareOperator::Eq,
        Box::new(Expr::Value(Value::String("1".to_owned()))),
    );
    let query = ODataQuery::new().with_filter(filter);

    let err = store
        .list(common::fixture_gts_id(VCPU_GTS), &query, &[])
        .await
        .expect_err("list must reject a non-allowlisted filter field");
    assert!(
        matches!(err, UsageCollectorPluginError::Internal(_)),
        "expected Internal from list, got {err:?}"
    );

    let err = store
        .aggregate(
            common::fixture_gts_id(VCPU_GTS),
            &query,
            &[],
            AggregationSpec {
                op: AggregationOp::Sum,
                group_by: Vec::new(),
            },
        )
        .await
        .expect_err("aggregate must reject a non-allowlisted filter field");
    assert!(
        matches!(err, UsageCollectorPluginError::Internal(_)),
        "expected Internal from aggregate, got {err:?}"
    );
}
