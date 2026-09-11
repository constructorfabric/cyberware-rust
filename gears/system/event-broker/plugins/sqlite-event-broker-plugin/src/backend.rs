//! The `SQLite` implementation of the SDK's `EventBrokerBackend` trait.
//!
//! Implements the outbox-retry dedup the gear's `docs/DESIGN.md` specifies:
//! one `last_chain_sequence` per `(topic, partition)`, compared against the
//! producer chain-sequence field on each incoming event - not `event.id`.
//!
//! Knows nothing about specifications, the broker's `Storage` facade, cluster
//! coordination, or idempotency beyond its own outbox-retry safety net,
//! matching that document's "the backend knows nothing about cluster
//! coordination, notifications, idempotency, or subscriptions. It is pure
//! storage."

use async_trait::async_trait;
use event_broker_sdk::Sequence;
use event_broker_sdk::api::Position;
use event_broker_sdk::models::{Event, PartitionLeader, PartitionRange, TopicSegment};
use event_broker_sdk::{
    EventBrokerBackend, OutOfRange, RetentionReport, RetentionRequest, StorageBackendError,
};
use sea_orm::sea_query::{Expr, ExprTrait};
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set};
use toolkit_db::DBProvider;
use toolkit_db::secure::{SecureDeleteExt, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_security::{AccessScope, SecurityContext};

use crate::connection::open_event_log;
use crate::entity::{event as event_entity, partition_state};
use crate::error::db_err_from_scope;
use crate::options::EventLogPath;
use crate::sizing::stored_bytes;

const BACKEND_INSTANCE: &str = "event-broker-sqlite-backend";

/// Rows one retention pass may examine.
///
/// A pass is bounded so the tick that drives it stays predictable: a partition
/// that has gone far past its bounds is brought back over several passes rather
/// than in one transaction holding a write lock for as long as it takes.
/// Removal is a prefix either way, so a partial pass leaves the partition in
/// exactly the shape a complete one would, only less far along.
const MAX_ROWS_PER_RETENTION_PASS: u64 = 4096;

fn persist_failed(reason: impl std::fmt::Display) -> StorageBackendError {
    StorageBackendError::PersistFailed {
        reason: reason.to_string(),
        detail: String::new(),
        instance: BACKEND_INSTANCE.to_owned(),
    }
}

fn read_failed(reason: impl std::fmt::Display) -> StorageBackendError {
    StorageBackendError::ReadFailed {
        reason: reason.to_string(),
        detail: String::new(),
        instance: BACKEND_INSTANCE.to_owned(),
    }
}

fn retention_failed(reason: impl std::fmt::Display) -> StorageBackendError {
    StorageBackendError::RetentionFailed {
        reason: reason.to_string(),
        detail: String::new(),
        instance: BACKEND_INSTANCE.to_owned(),
    }
}

pub struct SqliteEventBackend {
    db: std::sync::Arc<DBProvider<toolkit_db::DbError>>,
}

impl SqliteEventBackend {
    /// Opens the event log the given location names, applies this backend's
    /// tables to it, and returns a backend over it.
    ///
    /// The only way to build one: the database is this backend's own, never the
    /// host gear's, so there is no handle for a caller to hand in.
    ///
    /// # Errors
    /// [`StorageBackendError::Unavailable`] if the event log cannot be opened
    /// or its tables cannot be applied.
    pub async fn open(path: &EventLogPath) -> Result<Self, StorageBackendError> {
        Ok(Self {
            db: open_event_log(path).await?,
        })
    }
}

#[async_trait]
impl EventBrokerBackend for SqliteEventBackend {
    async fn persist(
        &self,
        _ctx: &SecurityContext,
        topic: &str,
        partition: u32,
        events: &[Event],
    ) -> Result<(), StorageBackendError> {
        if events.is_empty() {
            return Ok(());
        }
        let partition_i32 = i32::try_from(partition)
            .map_err(|e| persist_failed(format!("partition too large: {e}")))?;
        let topic = topic.to_owned();
        let events = events.to_vec();

        self.db
            .transaction(move |tx| {
                Box::pin(async move {
                    let state = partition_state::Entity::find()
                        .filter(partition_state::Column::Topic.eq(topic.clone()))
                        .filter(partition_state::Column::Partition.eq(partition_i32))
                        .secure()
                        .scope_with(&AccessScope::allow_all())
                        .one(tx)
                        .await
                        .map_err(|e| db_err_from_scope(&e))?;

                    let (mut next_sequence, last_chain_sequence) = match &state {
                        Some(row) => (row.next_sequence, row.last_chain_sequence),
                        None => (1, None),
                    };

                    // Outbox-retry dedup: compare every incoming event's
                    // producer chain sequence against the stored
                    // `last_chain_sequence` - not `event.id`. Events with no
                    // chain sequence (stateless mode) are never deduped here,
                    // matching stateless mode's documented "no broker-side
                    // dedup".
                    let chain_sequences: Vec<i64> = events
                        .iter()
                        .filter_map(|e| e.meta.as_ref().and_then(|m| m.sequence))
                        .collect();
                    let is_retry = last_chain_sequence.is_some_and(|last| {
                        !chain_sequences.is_empty() && chain_sequences.iter().all(|&s| s <= last)
                    });
                    if is_retry {
                        // Every chained/monotonic event in this batch was
                        // already durably applied - a retried persist call.
                        return Ok(());
                    }

                    let mut new_last_chain_sequence = last_chain_sequence;
                    // Counted here, in the same transaction that inserts the
                    // rows, so the partition's figures can never disagree with
                    // what is stored.
                    let mut added_events: i64 = 0;
                    let mut added_bytes: i64 = 0;
                    let mut models = Vec::with_capacity(events.len());
                    for event in &events {
                        let sequence = next_sequence;
                        next_sequence += 1;
                        if let Some(chain_sequence) = event.meta.as_ref().and_then(|m| m.sequence) {
                            new_last_chain_sequence = Some(chain_sequence);
                        }
                        let data = event.data.clone().unwrap_or(serde_json::Value::Null);
                        let row_bytes = stored_bytes(event);
                        added_events += 1;
                        added_bytes = added_bytes.saturating_add(row_bytes);
                        models.push(event_entity::ActiveModel {
                            id: Set(event.id),
                            type_id: Set(event.type_id.clone()),
                            topic: Set(topic.clone()),
                            tenant_id: Set(event.tenant_id),
                            source: Set(event.source.clone()),
                            subject: Set(event.subject.clone()),
                            subject_type: Set(event.subject_type.clone()),
                            occurred_at: Set(event.occurred_at),
                            trace_parent: Set(event.trace_parent.clone()),
                            data: Set(data),
                            partition: Set(partition_i32),
                            sequence: Set(sequence),
                            sequence_time: Set(chrono::Utc::now()),
                            stored_bytes: Set(row_bytes),
                        });
                    }

                    for model in models {
                        secure_insert::<event_entity::Entity>(model, &AccessScope::allow_all(), tx)
                            .await
                            .map_err(|e| db_err_from_scope(&e))?;
                    }

                    // Plain `ActiveModelTrait::update`/`insert` need
                    // `ConnectionTrait`, which `DbTx` deliberately does not
                    // implement (toolkit-db's "hidden database runner
                    // capability" security model) - `update_many().secure()`
                    // + `secure_insert` fallback works with `&DbTx` directly.
                    let update_result = partition_state::Entity::update_many()
                        .secure()
                        .scope_with(&AccessScope::allow_all())
                        .filter(
                            Condition::all()
                                .add(partition_state::Column::Topic.eq(topic.clone()))
                                .add(partition_state::Column::Partition.eq(partition_i32)),
                        )
                        .col_expr(
                            partition_state::Column::NextSequence,
                            Expr::value(next_sequence),
                        )
                        .col_expr(
                            partition_state::Column::LastChainSequence,
                            Expr::value(new_last_chain_sequence),
                        )
                        .col_expr(
                            partition_state::Column::EventCount,
                            Expr::col(partition_state::Column::EventCount).add(added_events),
                        )
                        .col_expr(
                            partition_state::Column::StoredBytes,
                            Expr::col(partition_state::Column::StoredBytes).add(added_bytes),
                        )
                        .exec(tx)
                        .await
                        .map_err(|e| db_err_from_scope(&e))?;
                    if update_result.rows_affected == 0 {
                        let am = partition_state::ActiveModel {
                            topic: Set(topic.clone()),
                            partition: Set(partition_i32),
                            next_sequence: Set(next_sequence),
                            last_chain_sequence: Set(new_last_chain_sequence),
                            event_count: Set(added_events),
                            stored_bytes: Set(added_bytes),
                        };
                        secure_insert::<partition_state::Entity>(am, &AccessScope::allow_all(), tx)
                            .await
                            .map_err(|e| db_err_from_scope(&e))?;
                    }
                    Ok(())
                })
            })
            .await
            .map_err(persist_failed)
    }

    async fn resolve(
        &self,
        _ctx: &SecurityContext,
        topic: &str,
        partition: u32,
        position: Position,
    ) -> Result<Sequence, StorageBackendError> {
        let partition_i32 = i32::try_from(partition)
            .map_err(|e| read_failed(format!("partition too large: {e}")))?;
        let conn = self.db.conn().map_err(|e| read_failed(e.to_string()))?;

        // The ceiling is the highest sequence ever *assigned*, so erasing the
        // newest event never lowers it. `next_sequence` is this backend's own
        // counter, not a position, so the step happens before the value becomes
        // a `Sequence`.
        let assigned = partition_state::Entity::find()
            .filter(partition_state::Column::Topic.eq(topic))
            .filter(partition_state::Column::Partition.eq(partition_i32))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .one(&conn)
            .await
            .map_err(|e| read_failed(e.to_string()))?
            .map_or(0, |state| state.next_sequence);
        let ceiling = Sequence::assigned(std::cmp::max(assigned.saturating_sub(1), 0));

        if matches!(position, Position::Latest) {
            return Ok(ceiling);
        }

        // The floor is one below the oldest sequence still stored. Nothing
        // retained leaves the ceiling as the only position a cursor can hold:
        // no event remains for delivery to resume from.
        let oldest = event_entity::Entity::find()
            .filter(event_entity::Column::Topic.eq(topic))
            .filter(event_entity::Column::Partition.eq(partition_i32))
            .order_by_asc(event_entity::Column::Sequence)
            .limit(1)
            .secure()
            .scope_with(&AccessScope::allow_all())
            .one(&conn)
            .await
            .map_err(|e| read_failed(e.to_string()))?
            .map(|row| row.sequence);
        let floor = oldest.map_or(ceiling, |seq| Sequence::assigned(seq.saturating_sub(1)));

        match position {
            Position::Latest => Ok(ceiling),
            Position::Earliest => Ok(floor),
            Position::At(at) => {
                // Bounded by the (topic, partition, occurred_at) index, never a
                // partition scan.
                let found = event_entity::Entity::find()
                    .filter(event_entity::Column::Topic.eq(topic))
                    .filter(event_entity::Column::Partition.eq(partition_i32))
                    .filter(event_entity::Column::OccurredAt.gte(at))
                    .order_by_asc(event_entity::Column::OccurredAt)
                    .limit(1)
                    .secure()
                    .scope_with(&AccessScope::allow_all())
                    .one(&conn)
                    .await
                    .map_err(|e| read_failed(e.to_string()))?
                    .map(|row| row.sequence);
                // Nothing that recent means only what arrives next, which is
                // what `Latest` says.
                Ok(found.map_or(ceiling, |seq| Sequence::assigned(seq.saturating_sub(1))))
            }
            Position::Exact(requested) => {
                if (floor..=ceiling).contains(&requested) {
                    Ok(requested)
                } else {
                    Err(StorageBackendError::OffsetOutOfRange {
                        floor,
                        ceiling,
                        breached: if requested < floor {
                            OutOfRange::BelowFloor
                        } else {
                            OutOfRange::AboveCeiling
                        },
                        detail: format!("valid positions are [{floor}, {ceiling}]"),
                        instance: String::new(),
                    })
                }
            }
        }
    }

    async fn read(
        &self,
        _ctx: &SecurityContext,
        topic: &str,
        partition: u32,
        after: Sequence,
        max_count: usize,
    ) -> Result<Vec<Event>, StorageBackendError> {
        let partition_i32 = i32::try_from(partition)
            .map_err(|e| read_failed(format!("partition too large: {e}")))?;
        let conn = self.db.conn().map_err(|e| read_failed(e.to_string()))?;
        let limit = u64::try_from(max_count).unwrap_or(u64::MAX);
        let rows = event_entity::Entity::find()
            .filter(event_entity::Column::Topic.eq(topic))
            .filter(event_entity::Column::Partition.eq(partition_i32))
            .filter(event_entity::Column::Sequence.gt(after.as_i64()))
            .order_by_asc(event_entity::Column::Sequence)
            .limit(limit)
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .map_err(|e| read_failed(e.to_string()))?;
        Ok(rows.into_iter().map(row_to_sdk_event).collect())
    }

    async fn query(
        &self,
        _ctx: &SecurityContext,
        topic: &str,
        partition: u32,
        range: PartitionRange,
    ) -> Result<Vec<TopicSegment>, StorageBackendError> {
        let partition_i32 = i32::try_from(partition)
            .map_err(|e| read_failed(format!("partition too large: {e}")))?;
        let conn = self.db.conn().map_err(|e| read_failed(e.to_string()))?;
        let mut query = event_entity::Entity::find()
            .filter(event_entity::Column::Topic.eq(topic))
            .filter(event_entity::Column::Partition.eq(partition_i32));
        if let Some(start) = range.start_offset {
            query = query.filter(event_entity::Column::Sequence.gte(start.as_i64()));
        }
        if let Some(end) = range.end_offset {
            query = query.filter(event_entity::Column::Sequence.lte(end.as_i64()));
        }
        let rows = query
            .order_by_asc(event_entity::Column::Sequence)
            .limit(u64::from(range.limit))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .map_err(|e| read_failed(e.to_string()))?;

        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let start_sequence = rows
            .first()
            .map_or(Sequence::NONE, |r| Sequence::assigned(r.sequence));
        let end_sequence = rows
            .last()
            .map_or(Sequence::NONE, |r| Sequence::assigned(r.sequence));
        let start_time = rows.first().map(|r| r.sequence_time);
        let end_time = rows.last().map(|r| r.sequence_time);
        // One synthetic segment describing the whole matched range - honest
        // for what this backend actually does (no real segmented storage;
        // the broker's `docs/openapi.yaml`: "Backend-specific in shape").
        Ok(vec![TopicSegment {
            topic: topic.to_owned(),
            partition,
            start_sequence,
            end_sequence,
            start_time,
            end_time,
            segments: vec![serde_json::json!({
                "start_sequence": start_sequence,
                "end_sequence": end_sequence,
                "event_count": rows.len(),
            })],
        }])
    }

    async fn list_partition_leaders(
        &self,
        _ctx: &SecurityContext,
        topic: &str,
    ) -> Result<Vec<PartitionLeader>, StorageBackendError> {
        // Standalone mode has no sharding - this single process is the only
        // leader for every partition of every topic. `endpoint` is empty
        // rather than a real address since nothing calls this in standalone
        // mode today (it exists for a future cluster-mode dispatcher).
        let conn = self.db.conn().map_err(|e| read_failed(e.to_string()))?;
        let partitions: std::collections::BTreeSet<i32> = event_entity::Entity::find()
            .filter(event_entity::Column::Topic.eq(topic))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .map_err(|e| read_failed(e.to_string()))?
            .into_iter()
            .map(|r| r.partition)
            .collect();
        Ok(partitions
            .into_iter()
            .filter_map(|p| u32::try_from(p).ok())
            .map(|partition| PartitionLeader {
                partition,
                endpoint: String::new(),
            })
            .collect())
    }

    async fn maintain(
        &self,
        _ctx: &SecurityContext,
        request: &RetentionRequest,
    ) -> Result<RetentionReport, StorageBackendError> {
        let partition_i32 = i32::try_from(request.partition())
            .map_err(|e| retention_failed(format!("partition too large: {e}")))?;
        let topic = request.topic().to_owned();
        let oldest_permitted = request.oldest_permitted();
        let max_stored_bytes = request.max_stored_bytes();

        self.db
            .transaction(move |tx| {
                Box::pin(async move {
                    let Some(state) = partition_state::Entity::find()
                        .filter(partition_state::Column::Topic.eq(topic.clone()))
                        .filter(partition_state::Column::Partition.eq(partition_i32))
                        .secure()
                        .scope_with(&AccessScope::allow_all())
                        .one(tx)
                        .await
                        .map_err(|e| db_err_from_scope(&e))?
                    else {
                        // Nothing has ever been stored here, so there is
                        // nothing to bound.
                        return Ok(RetentionReport::default());
                    };

                    // Ascending by sequence, because removal takes a prefix:
                    // what survives has to be a suffix of what was stored, and
                    // walking any other order could leave a hole between two
                    // retained events.
                    let candidates = event_entity::Entity::find()
                        .filter(event_entity::Column::Topic.eq(topic.clone()))
                        .filter(event_entity::Column::Partition.eq(partition_i32))
                        .order_by_asc(event_entity::Column::Sequence)
                        .limit(MAX_ROWS_PER_RETENTION_PASS)
                        .secure()
                        .scope_with(&AccessScope::allow_all())
                        .all(tx)
                        .await
                        .map_err(|e| db_err_from_scope(&e))?;

                    let mut removed_events: i64 = 0;
                    let mut removed_bytes: i64 = 0;
                    let mut highest_removed: Option<i64> = None;
                    for row in &candidates {
                        let bytes_if_kept = state.stored_bytes.saturating_sub(removed_bytes);
                        let past_duration = row.sequence_time < oldest_permitted;
                        let past_size = max_stored_bytes.is_some_and(|max| {
                            u64::try_from(bytes_if_kept).unwrap_or(u64::MAX) > max
                        });
                        // The first row that is within both bounds ends the
                        // pass. Stopping here rather than skipping it is what
                        // keeps removal a prefix: the bounds are a property of
                        // the partition, and one event that escaped them does
                        // not license punching a hole through the middle of it.
                        if !past_duration && !past_size {
                            break;
                        }
                        removed_events += 1;
                        removed_bytes = removed_bytes.saturating_add(row.stored_bytes);
                        highest_removed = Some(row.sequence);
                    }

                    if let Some(floor) = highest_removed {
                        let deleted = event_entity::Entity::delete_many()
                            .secure()
                            .scope_with(&AccessScope::allow_all())
                            .filter(
                                Condition::all()
                                    .add(event_entity::Column::Topic.eq(topic.clone()))
                                    .add(event_entity::Column::Partition.eq(partition_i32))
                                    .add(event_entity::Column::Sequence.lte(floor)),
                            )
                            .exec(tx)
                            .await
                            .map_err(|e| db_err_from_scope(&e))?;
                        // The rows the database actually removed, not the rows
                        // the walk expected to remove. Counting the real thing
                        // is the point.
                        removed_events = i64::try_from(deleted.rows_affected).unwrap_or(i64::MAX);

                        partition_state::Entity::update_many()
                            .secure()
                            .scope_with(&AccessScope::allow_all())
                            .filter(
                                Condition::all()
                                    .add(partition_state::Column::Topic.eq(topic.clone()))
                                    .add(partition_state::Column::Partition.eq(partition_i32)),
                            )
                            .col_expr(
                                partition_state::Column::EventCount,
                                Expr::col(partition_state::Column::EventCount).sub(removed_events),
                            )
                            .col_expr(
                                partition_state::Column::StoredBytes,
                                Expr::col(partition_state::Column::StoredBytes).sub(removed_bytes),
                            )
                            .exec(tx)
                            .await
                            .map_err(|e| db_err_from_scope(&e))?;
                    }

                    let oldest_surviving = event_entity::Entity::find()
                        .filter(event_entity::Column::Topic.eq(topic.clone()))
                        .filter(event_entity::Column::Partition.eq(partition_i32))
                        .order_by_asc(event_entity::Column::Sequence)
                        .limit(1)
                        .secure()
                        .scope_with(&AccessScope::allow_all())
                        .one(tx)
                        .await
                        .map_err(|e| db_err_from_scope(&e))?
                        .map(|row| row.sequence);

                    Ok(RetentionReport {
                        removed_events: u64::try_from(removed_events).unwrap_or(0),
                        removed_bytes: u64::try_from(removed_bytes).unwrap_or(0),
                        remaining_events: u64::try_from(
                            state.event_count.saturating_sub(removed_events),
                        )
                        .unwrap_or(0),
                        remaining_bytes: u64::try_from(
                            state.stored_bytes.saturating_sub(removed_bytes),
                        )
                        .unwrap_or(0),
                        oldest_surviving_sequence: oldest_surviving.map(Sequence::assigned),
                    })
                })
            })
            .await
            .map_err(retention_failed)
    }
}

fn row_to_sdk_event(row: event_entity::Model) -> Event {
    Event {
        id: row.id,
        type_id: row.type_id,
        tenant_id: row.tenant_id,
        source: row.source,
        subject: row.subject,
        subject_type: row.subject_type,
        occurred_at: row.occurred_at,
        trace_parent: row.trace_parent,
        data: Some(row.data),
        partition: u32::try_from(row.partition).ok(),
        sequence: Some(Sequence::assigned(row.sequence)),
        sequence_time: Some(row.sequence_time),
        meta: None,
    }
}

#[cfg(test)]
#[path = "backend_tests.rs"]
mod backend_tests;
