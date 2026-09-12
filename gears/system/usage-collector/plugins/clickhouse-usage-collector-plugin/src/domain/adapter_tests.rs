use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use toolkit_odata::{ODataQuery, Page as ODataPage, PageInfo};
use uuid::Uuid;

use usage_collector_sdk::{
    AggregationOp, AggregationResult, AggregationSpec, MetadataFilter, UsageCollectorPluginError,
    UsageCollectorPluginV1, UsageKind, UsageRecord, UsageType, UsageTypeGtsId,
};

use super::StorageAdapter;
use crate::domain::ports::{CatalogStore, RecordStore};

#[derive(Default)]
struct CallLog {
    record: Mutex<Vec<&'static str>>,
    catalog: Mutex<Vec<&'static str>>,
}

struct MockRecord {
    log: Arc<CallLog>,
}

struct MockCatalog {
    log: Arc<CallLog>,
}

fn empty_page<T>() -> ODataPage<T> {
    ODataPage::new(
        vec![],
        PageInfo {
            next_cursor: None,
            prev_cursor: None,
            limit: 0,
        },
    )
}

#[async_trait]
impl RecordStore for MockRecord {
    async fn create(&self, record: UsageRecord) -> Result<UsageRecord, UsageCollectorPluginError> {
        self.log.record.lock().unwrap().push("create");
        Ok(record)
    }

    async fn create_batch(
        &self,
        records: Vec<UsageRecord>,
    ) -> Result<Vec<Result<UsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>
    {
        self.log.record.lock().unwrap().push("create_batch");
        Ok(records.into_iter().map(Ok).collect())
    }

    async fn get(&self, _id: Uuid) -> Result<UsageRecord, UsageCollectorPluginError> {
        self.log.record.lock().unwrap().push("get");
        Err(UsageCollectorPluginError::UsageRecordNotFound { id: Uuid::nil() })
    }

    async fn list(
        &self,
        _gts_id: UsageTypeGtsId,
        _query: &ODataQuery,
        _metadata_filter: &[MetadataFilter],
    ) -> Result<ODataPage<UsageRecord>, UsageCollectorPluginError> {
        self.log.record.lock().unwrap().push("list");
        Ok(empty_page())
    }

    async fn aggregate(
        &self,
        _gts_id: UsageTypeGtsId,
        _query: &ODataQuery,
        _metadata_filter: &[MetadataFilter],
        _spec: AggregationSpec,
    ) -> Result<AggregationResult, UsageCollectorPluginError> {
        self.log.record.lock().unwrap().push("aggregate");
        Ok(AggregationResult { buckets: vec![] })
    }

    async fn deactivate(&self, _id: Uuid) -> Result<(), UsageCollectorPluginError> {
        self.log.record.lock().unwrap().push("deactivate");
        Ok(())
    }
}

#[async_trait]
impl CatalogStore for MockCatalog {
    async fn create(&self, usage_type: UsageType) -> Result<UsageType, UsageCollectorPluginError> {
        self.log.catalog.lock().unwrap().push("create");
        Ok(usage_type)
    }

    async fn get(&self, gts_id: UsageTypeGtsId) -> Result<UsageType, UsageCollectorPluginError> {
        self.log.catalog.lock().unwrap().push("get");
        Err(UsageCollectorPluginError::UsageTypeNotFound { gts_id })
    }

    async fn list(
        &self,
        _query: &ODataQuery,
    ) -> Result<ODataPage<UsageType>, UsageCollectorPluginError> {
        self.log.catalog.lock().unwrap().push("list");
        Ok(empty_page())
    }

    async fn delete(&self, _gts_id: UsageTypeGtsId) -> Result<(), UsageCollectorPluginError> {
        self.log.catalog.lock().unwrap().push("delete");
        Ok(())
    }
}

fn adapter_with_log() -> (StorageAdapter, Arc<CallLog>) {
    let log = Arc::new(CallLog::default());
    let adapter = StorageAdapter::new(
        Arc::new(MockRecord {
            log: Arc::clone(&log),
        }),
        Arc::new(MockCatalog {
            log: Arc::clone(&log),
        }),
    );
    (adapter, log)
}

fn sample_gts() -> UsageTypeGtsId {
    UsageTypeGtsId::new("gts.cf.core.uc.usage_record.v1~cf.compute._.vcpu_hours.v1").unwrap()
}

fn sample_agg_spec() -> AggregationSpec {
    AggregationSpec {
        op: AggregationOp::Sum,
        group_by: vec![],
    }
}

#[tokio::test]
async fn create_usage_record_delegates_to_record_store() {
    use std::collections::BTreeMap;

    use time::OffsetDateTime;
    use usage_collector_sdk::{IdempotencyKey, ResourceRef, UsageRecordStatus};

    let (adapter, log) = adapter_with_log();
    let record = UsageRecord {
        id: Uuid::from_u128(1),
        tenant_id: Uuid::from_u128(2),
        gts_id: sample_gts(),
        value: rust_decimal::Decimal::new(1, 0),
        created_at: OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
        resource_ref: ResourceRef::new("r".to_owned(), "t".to_owned()).unwrap(),
        subject_ref: None,
        idempotency_key: IdempotencyKey::new("k".to_owned()).unwrap(),
        corrects_id: None,
        status: UsageRecordStatus::Active,
        metadata: BTreeMap::new(),
    };
    adapter.create_usage_record(record).await.unwrap();
    assert_eq!(*log.record.lock().unwrap(), vec!["create"]);
}

#[tokio::test]
async fn create_usage_records_delegates_to_record_store() {
    let (adapter, log) = adapter_with_log();
    adapter.create_usage_records(vec![]).await.unwrap();
    assert_eq!(*log.record.lock().unwrap(), vec!["create_batch"]);
}

#[tokio::test]
async fn get_usage_record_delegates_to_record_store() {
    let (adapter, log) = adapter_with_log();
    let err = adapter
        .get_usage_record(Uuid::nil())
        .await
        .expect_err("the mock record store reports not-found");
    assert!(
        matches!(err, UsageCollectorPluginError::UsageRecordNotFound { .. }),
        "the store's error must reach the caller unchanged, got {err:?}"
    );
    assert_eq!(*log.record.lock().unwrap(), vec!["get"]);
}

#[tokio::test]
async fn query_aggregated_usage_records_delegates_to_record_store() {
    let (adapter, log) = adapter_with_log();
    adapter
        .query_aggregated_usage_records(
            sample_gts(),
            &ODataQuery::default(),
            &[],
            sample_agg_spec(),
        )
        .await
        .unwrap();
    assert_eq!(*log.record.lock().unwrap(), vec!["aggregate"]);
}

#[tokio::test]
async fn list_usage_records_delegates_to_record_store() {
    let (adapter, log) = adapter_with_log();
    adapter
        .list_usage_records(sample_gts(), &ODataQuery::default(), &[])
        .await
        .unwrap();
    assert_eq!(*log.record.lock().unwrap(), vec!["list"]);
}

#[tokio::test]
async fn deactivate_usage_record_delegates_to_record_store() {
    let (adapter, log) = adapter_with_log();
    adapter.deactivate_usage_record(Uuid::nil()).await.unwrap();
    assert_eq!(*log.record.lock().unwrap(), vec!["deactivate"]);
}

#[tokio::test]
async fn create_usage_type_delegates_to_catalog_store() {
    use std::collections::BTreeSet;

    let (adapter, log) = adapter_with_log();
    let usage_type = UsageType {
        gts_id: sample_gts(),
        kind: UsageKind::Counter,
        metadata_fields: BTreeSet::new(),
    };
    adapter.create_usage_type(usage_type).await.unwrap();
    assert_eq!(*log.catalog.lock().unwrap(), vec!["create"]);
}

#[tokio::test]
async fn get_usage_type_delegates_to_catalog_store() {
    let (adapter, log) = adapter_with_log();
    let err = adapter
        .get_usage_type(sample_gts())
        .await
        .expect_err("the mock catalog store reports not-found");
    assert!(
        matches!(err, UsageCollectorPluginError::UsageTypeNotFound { .. }),
        "the store's error must reach the caller unchanged, got {err:?}"
    );
    assert_eq!(*log.catalog.lock().unwrap(), vec!["get"]);
}

#[tokio::test]
async fn list_usage_types_delegates_to_catalog_store() {
    let (adapter, log) = adapter_with_log();
    adapter
        .list_usage_types(&ODataQuery::default())
        .await
        .unwrap();
    assert_eq!(*log.catalog.lock().unwrap(), vec!["list"]);
}

#[tokio::test]
async fn delete_usage_type_delegates_to_catalog_store() {
    let (adapter, log) = adapter_with_log();
    adapter.delete_usage_type(sample_gts()).await.unwrap();
    assert_eq!(*log.catalog.lock().unwrap(), vec!["delete"]);
}
