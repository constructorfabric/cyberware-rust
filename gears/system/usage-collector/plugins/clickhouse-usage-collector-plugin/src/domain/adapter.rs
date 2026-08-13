use std::sync::Arc;

use async_trait::async_trait;
use toolkit_macros::domain_model;
use toolkit_odata::{ODataQuery, Page as ODataPage};
use uuid::Uuid;

use usage_collector_sdk::{
    AggregationResult, AggregationSpec, MetadataFilter, UsageCollectorPluginError,
    UsageCollectorPluginV1, UsageRecord, UsageType, UsageTypeGtsId,
};

use crate::domain::ports::{CatalogStore, RecordStore};

/// The single implementation of [`UsageCollectorPluginV1`]. Delegates record ops
/// to the [`RecordStore`] port and catalog ops to the [`CatalogStore`] port.
#[domain_model]
pub(crate) struct StorageAdapter {
    record: Arc<dyn RecordStore>,
    catalog: Arc<dyn CatalogStore>,
}

impl StorageAdapter {
    #[must_use]
    pub(crate) fn new(record: Arc<dyn RecordStore>, catalog: Arc<dyn CatalogStore>) -> Self {
        Self { record, catalog }
    }
}

#[async_trait]
impl UsageCollectorPluginV1 for StorageAdapter {
    async fn create_usage_record(
        &self,
        record: UsageRecord,
    ) -> Result<UsageRecord, UsageCollectorPluginError> {
        self.record.create(record).await
    }

    async fn create_usage_records(
        &self,
        records: Vec<UsageRecord>,
    ) -> Result<Vec<Result<UsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>
    {
        self.record.create_batch(records).await
    }

    async fn get_usage_record(&self, id: Uuid) -> Result<UsageRecord, UsageCollectorPluginError> {
        self.record.get(id).await
    }

    async fn query_aggregated_usage_records(
        &self,
        gts_id: UsageTypeGtsId,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        aggregation: AggregationSpec,
    ) -> Result<AggregationResult, UsageCollectorPluginError> {
        self.record
            .aggregate(gts_id, query, metadata_filter, aggregation)
            .await
    }

    async fn list_usage_records(
        &self,
        gts_id: UsageTypeGtsId,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
    ) -> Result<ODataPage<UsageRecord>, UsageCollectorPluginError> {
        self.record.list(gts_id, query, metadata_filter).await
    }

    async fn deactivate_usage_record(&self, id: Uuid) -> Result<(), UsageCollectorPluginError> {
        self.record.deactivate(id).await
    }

    async fn create_usage_type(
        &self,
        usage_type: UsageType,
    ) -> Result<UsageType, UsageCollectorPluginError> {
        self.catalog.create(usage_type).await
    }

    async fn get_usage_type(
        &self,
        gts_id: UsageTypeGtsId,
    ) -> Result<UsageType, UsageCollectorPluginError> {
        self.catalog.get(gts_id).await
    }

    async fn list_usage_types(
        &self,
        query: &ODataQuery,
    ) -> Result<ODataPage<UsageType>, UsageCollectorPluginError> {
        self.catalog.list(query).await
    }

    async fn delete_usage_type(
        &self,
        gts_id: UsageTypeGtsId,
    ) -> Result<(), UsageCollectorPluginError> {
        self.catalog.delete(gts_id).await
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "adapter_tests.rs"]
mod adapter_tests;
