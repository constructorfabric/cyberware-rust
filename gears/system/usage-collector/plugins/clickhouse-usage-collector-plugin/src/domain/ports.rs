use async_trait::async_trait;
use toolkit_odata::{ODataQuery, Page as ODataPage};
use uuid::Uuid;

use usage_collector_sdk::{
    AggregationResult, AggregationSpec, MetadataFilter, UsageCollectorPluginError, UsageRecord,
    UsageType, UsageTypeGtsId,
};

/// Persistence + query operations on `usage_records`. Implemented by infra.
#[async_trait]
pub trait RecordStore: Send + Sync + 'static {
    async fn create(&self, record: UsageRecord) -> Result<UsageRecord, UsageCollectorPluginError>;
    async fn create_batch(
        &self,
        records: Vec<UsageRecord>,
    ) -> Result<Vec<Result<UsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>;
    async fn get(&self, id: Uuid) -> Result<UsageRecord, UsageCollectorPluginError>;
    async fn list(
        &self,
        gts_id: UsageTypeGtsId,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
    ) -> Result<ODataPage<UsageRecord>, UsageCollectorPluginError>;
    async fn aggregate(
        &self,
        gts_id: UsageTypeGtsId,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        spec: AggregationSpec,
    ) -> Result<AggregationResult, UsageCollectorPluginError>;
    async fn deactivate(&self, id: Uuid) -> Result<(), UsageCollectorPluginError>;
}

/// Catalog operations on `usage_type_catalog`. Implemented by infra (Catalog Store).
#[async_trait]
pub trait CatalogStore: Send + Sync + 'static {
    async fn create(&self, usage_type: UsageType) -> Result<UsageType, UsageCollectorPluginError>;
    async fn get(&self, gts_id: UsageTypeGtsId) -> Result<UsageType, UsageCollectorPluginError>;
    async fn list(
        &self,
        query: &ODataQuery,
    ) -> Result<ODataPage<UsageType>, UsageCollectorPluginError>;
    async fn delete(&self, gts_id: UsageTypeGtsId) -> Result<(), UsageCollectorPluginError>;
}
