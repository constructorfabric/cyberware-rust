use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
#[cfg(feature = "db")]
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, Set, sea_query::OnConflict};
#[cfg(feature = "db")]
use toolkit_db::secure::{AccessScope, SecureEntityExt, SecureInsertExt};

use crate::api::ResolvedPosition;
use crate::error::OffsetManagerError;
use crate::ids::{ConsumerGroupId, TopicId};

type OffsetKey = (ConsumerGroupId, TopicId, u32);
type PartitionOffsetKey = (TopicId, u32);
type CommittedOffsets = HashMap<OffsetKey, i64>;
type SeedOffsets = HashMap<PartitionOffsetKey, i64>;

#[cfg(feature = "db")]
mod offset_row {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "evbk_consumer_offsets")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub consumer_group_id: Uuid,
        #[sea_orm(primary_key, auto_increment = false)]
        pub topic_id: Uuid,
        #[sea_orm(primary_key, auto_increment = false)]
        pub partition: i32,
        pub offset: i64,
        pub updated_at: DateTimeUtc,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}

    impl toolkit_db::secure::ScopableEntity for Entity {
        const IS_UNRESTRICTED: bool = true;

        fn tenant_col() -> Option<Self::Column> {
            None
        }

        fn resource_col() -> Option<Self::Column> {
            None
        }

        fn owner_col() -> Option<Self::Column> {
            None
        }

        fn type_col() -> Option<Self::Column> {
            None
        }

        fn resolve_property(_property: &str) -> Option<Self::Column> {
            None
        }
        fn scope_columns() -> Vec<Self::Column> {
            Vec::new()
        }
    }
}

/// Policy applied when no committed cursor exists for an assigned partition.
/// Required argument to the constructors of all built-in offset stores.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fallback {
    Earliest,
    Latest,
}

impl From<Fallback> for ResolvedPosition {
    fn from(f: Fallback) -> Self {
        match f {
            Fallback::Earliest => ResolvedPosition::Earliest,
            Fallback::Latest => ResolvedPosition::Latest,
        }
    }
}

#[cfg(feature = "db")]
pub const LOCAL_DB_OFFSET_STORE_MIGRATION_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS evbk_consumer_offsets (
    consumer_group_id UUID NOT NULL,
    topic_id UUID NOT NULL,
    partition INTEGER NOT NULL,
    offset BIGINT NOT NULL,
    updated_at TIMESTAMP NOT NULL,
    PRIMARY KEY (consumer_group_id, topic_id, partition)
);
"#;

// ---- Traits ------------------------------------------------------------------
//
// Offsets are a *client-side* concern: the consumer owns durable progress, and
// SEEK positions broker runtime cursor state for a subscription session. Reading
// "where do I start?" is universal ([`OffsetStore`]); persisting a processed
// offset is a separate capability whose *mechanism* is the variable axis - an
// eventual/batched commit ([`CommitOffset`]) or a transactional commit atomic
// with the caller's DB writes ([`CommitOffsetInTx`]). A store implements exactly
// one commit flavor; there is no manager that carries two.

/// Resolves where each assigned `(group, topic, partition)` should start.
///
/// `load_position(...)` is the single source of truth for "where should this
/// partition begin?": an exact last-processed offset (verbatim from the backing
/// store or a configured per-partition override) or a sentinel for the broker to
/// resolve. Read-only - persistence is [`CommitOffset`] / [`CommitOffsetInTx`].
#[async_trait]
pub trait OffsetStore: Send + Sync {
    async fn load_position(
        &self,
        group: &ConsumerGroupId,
        topic: &TopicId,
        partition: u32,
    ) -> Result<ResolvedPosition, OffsetManagerError>;
}

/// Eventual / batched commit of a processed offset to the client store
/// (at-least-once). The dispatcher's auto-commit timer drives this.
#[async_trait]
pub trait CommitOffset: OffsetStore {
    /// Persist `offset` as the last-processed offset for `(group, topic, partition)`.
    async fn commit(
        &self,
        group: &ConsumerGroupId,
        topic: &TopicId,
        partition: u32,
        offset: i64,
    ) -> Result<(), OffsetManagerError>;
}

/// Transactional commit: persist the offset atomically within the caller's DB
/// transaction (exactly-once with the handler's business writes). Implemented
/// only by stores that can join a caller-supplied transaction (e.g.
/// [`LocalDbOffsetManager`]). Requires the `db` feature.
#[cfg(feature = "db")]
#[async_trait]
pub trait CommitOffsetInTx: OffsetStore {
    async fn commit_in_tx<TX>(
        &self,
        txn: &TX,
        group: &ConsumerGroupId,
        topic: &TopicId,
        partition: u32,
        offset: i64,
    ) -> Result<(), OffsetManagerError>
    where
        TX: toolkit_db::secure::DBRunner + Sync;
}

// ---- Built-in implementations ------------------------------------------------

/// DB-backed offset store. Implements [`OffsetStore`] + [`CommitOffsetInTx`] -
/// its purpose is the transactional commit (exactly-once with the handler's
/// writes). Cursor table: `evbk_consumer_offsets`.
/// Key: `(consumer_group_id UUID, topic_id UUID, partition INTEGER)`.
/// Requires the `db` feature.
#[cfg(feature = "db")]
pub struct LocalDbOffsetManager {
    db: toolkit_db::Db,
    fallback: Fallback,
    overrides: SeedOffsets,
}

#[cfg(feature = "db")]
impl LocalDbOffsetManager {
    pub fn new(db: toolkit_db::Db, fallback: Fallback) -> Self {
        Self {
            db,
            fallback,
            overrides: HashMap::new(),
        }
    }

    /// Per-partition seed offsets, consulted only when the DB has no row.
    pub fn with_overrides(
        mut self,
        overrides: impl IntoIterator<Item = (PartitionOffsetKey, i64)>,
    ) -> Self {
        self.overrides.extend(overrides);
        self
    }
}

#[cfg(feature = "db")]
#[async_trait]
impl OffsetStore for LocalDbOffsetManager {
    async fn load_position(
        &self,
        group: &ConsumerGroupId,
        topic: &TopicId,
        partition: u32,
    ) -> Result<ResolvedPosition, OffsetManagerError> {
        let conn = self.db.conn().map_err(|err| {
            OffsetManagerError::load_failed("open offset DB connection", err.to_string(), err)
        })?;

        let row = offset_row::Entity::find()
            .filter(offset_row::Column::ConsumerGroupId.eq(group.as_uuid()))
            .filter(offset_row::Column::TopicId.eq(topic.as_uuid()))
            .filter(offset_row::Column::Partition.eq(partition_to_i32(partition)?))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .one(&conn)
            .await
            .map_err(|err| {
                OffsetManagerError::load_failed("load offset row", err.to_string(), err)
            })?;

        if let Some(row) = row {
            return Ok(ResolvedPosition::Exact(row.offset));
        }

        if let Some(&off) = self.overrides.get(&(*topic, partition)) {
            return Ok(ResolvedPosition::Exact(off));
        }
        Ok(self.fallback.into())
    }
}

#[cfg(feature = "db")]
#[async_trait]
impl CommitOffsetInTx for LocalDbOffsetManager {
    async fn commit_in_tx<TX>(
        &self,
        txn: &TX,
        group: &ConsumerGroupId,
        topic: &TopicId,
        partition: u32,
        offset: i64,
    ) -> Result<(), OffsetManagerError>
    where
        TX: toolkit_db::secure::DBRunner + Sync,
    {
        let row = offset_row::ActiveModel {
            consumer_group_id: Set(group.as_uuid()),
            topic_id: Set(topic.as_uuid()),
            partition: Set(partition_to_i32(partition)?),
            offset: Set(offset),
            updated_at: Set(chrono::Utc::now()),
        };

        offset_row::Entity::insert(row)
            .secure()
            .scope_unchecked(&AccessScope::allow_all())
            .map_err(|err| {
                OffsetManagerError::persist_failed("scope offset upsert", err.to_string(), err)
            })?
            .on_conflict_raw(
                OnConflict::columns([
                    offset_row::Column::ConsumerGroupId,
                    offset_row::Column::TopicId,
                    offset_row::Column::Partition,
                ])
                .update_columns([offset_row::Column::Offset, offset_row::Column::UpdatedAt])
                .to_owned(),
            )
            .exec(txn)
            .await
            .map_err(|err| {
                OffsetManagerError::persist_failed("upsert offset row", err.to_string(), err)
            })?;
        Ok(())
    }
}

#[cfg(feature = "db")]
fn partition_to_i32(partition: u32) -> Result<i32, OffsetManagerError> {
    i32::try_from(partition).map_err(|_| {
        OffsetManagerError::Internal(format!("partition {partition} exceeds i32 storage range"))
    })
}

/// In-memory offset store for tests. Implements [`OffsetStore`] + [`CommitOffset`];
/// `commit` persists to a map (so the test can assert round-trip).
pub struct InMemoryOffsetManager {
    inner: Mutex<CommittedOffsets>,
    fallback: Fallback,
    overrides: SeedOffsets,
}

impl InMemoryOffsetManager {
    pub fn new(fallback: Fallback) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            fallback,
            overrides: HashMap::new(),
        }
    }

    pub fn with_overrides(
        mut self,
        overrides: impl IntoIterator<Item = (PartitionOffsetKey, i64)>,
    ) -> Self {
        self.overrides.extend(overrides);
        self
    }
}

#[async_trait]
impl OffsetStore for InMemoryOffsetManager {
    async fn load_position(
        &self,
        group: &ConsumerGroupId,
        topic: &TopicId,
        partition: u32,
    ) -> Result<ResolvedPosition, OffsetManagerError> {
        let guard = self
            .inner
            .lock()
            .map_err(|_| OffsetManagerError::Internal("mutex poisoned".into()))?;
        if let Some(&stored) = guard.get(&(*group, *topic, partition)) {
            return Ok(ResolvedPosition::Exact(stored));
        }
        drop(guard);
        if let Some(&off) = self.overrides.get(&(*topic, partition)) {
            return Ok(ResolvedPosition::Exact(off));
        }
        Ok(self.fallback.into())
    }
}

#[async_trait]
impl CommitOffset for InMemoryOffsetManager {
    async fn commit(
        &self,
        group: &ConsumerGroupId,
        topic: &TopicId,
        partition: u32,
        offset: i64,
    ) -> Result<(), OffsetManagerError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| OffsetManagerError::Internal("mutex poisoned".into()))?;
        let entry = guard.entry((*group, *topic, partition)).or_insert(0);
        *entry = (*entry).max(offset);
        Ok(())
    }
}
