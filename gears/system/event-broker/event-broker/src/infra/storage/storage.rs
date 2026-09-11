//! `Storage`: the real (SQLite + `ClusterCacheV1`) implementation of every
//! domain repo trait `InMemoryDomainRepo` used to stand in for, except
//! `TopicRepo`/`EventRepo` (topics go through `SpecificationManager` only;
//! events go straight to the backend - eb-single-process-implementation
//! D1/D3). One struct implementing every trait, matching
//! `InMemoryDomainRepo`'s own precedent (`domain/repo.rs`'s doc comment:
//! distinct method names across traits avoid call-site ambiguity).
//!
//! Namespace-to-engine split (design.md D2, corrected mid-design against
//! DESIGN.md's own "why subscription/group state is ephemeral" invariant):
//! `subscription` -> `ClusterCacheV1` (session-lifetime, TTL); `cursor`/
//! `consumer_group`/`producer_state` -> SQLite (durable). `cursor` and
//! `producer_sequence` denormalize `tenant_id` from their owning
//! `consumer_group`/`producer` row at write time, since neither has an
//! independent tenant of its own.

use crate::domain::model::Sequence;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use chrono::Utc;
use cluster_sdk::ClusterCacheV1;
use cluster_sdk::cache::{PutRequest, Ttl};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter, Set};
use toolkit_db::DBProvider;
use toolkit_db::outbox::Outbox;
use toolkit_db::secure::{SecureDeleteExt, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_gts::GtsInstanceId;
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::idempotency::{IdempotencyGuard, ProducerIdempotencyOutcome, PublishEnqueue};
use crate::domain::ingest::{
    ProducerCursors, ProducerMode, ProducerPartitionCursor, ProducerRegistration, ProducerRegistry,
    ProducerResetScope, ProducerTopicCursors,
};
use crate::domain::model::{ConsumerGroup, ConsumerGroupKind, Cursor, Subscription};
use crate::domain::notify::{DeliveryNotifier, NOTIFICATION_PREFIX};
use crate::domain::repo::{ConsumerGroupRepo, CursorRepo, SubscriptionRepo};
use crate::domain::specification::SpecificationManager;
use crate::infra::storage::entity::{consumer_group, cursor, producer, producer_sequence};
use crate::infra::storage::error::db_err_from_scope;

const SUBSCRIPTION_KEY_PREFIX: &str = "subscription/";

fn subscription_key(id: Uuid) -> String {
    format!("{SUBSCRIPTION_KEY_PREFIX}{id}")
}

fn kind_to_str(kind: ConsumerGroupKind) -> &'static str {
    match kind {
        ConsumerGroupKind::Anonymous => "anonymous",
        ConsumerGroupKind::Named => "named",
    }
}

fn str_to_kind(s: &str) -> Result<ConsumerGroupKind, DomainError> {
    match s {
        "anonymous" => Ok(ConsumerGroupKind::Anonymous),
        "named" => Ok(ConsumerGroupKind::Named),
        other => Err(DomainError::Internal(format!(
            "stored consumer_group.kind '{other}' is neither 'anonymous' nor 'named'"
        ))),
    }
}

fn row_to_consumer_group(row: consumer_group::Model) -> Result<ConsumerGroup, DomainError> {
    Ok(ConsumerGroup {
        id: parse_gts_id(&row.id)?,
        kind: str_to_kind(&row.kind)?,
        tenant_id: row.tenant_id,
        owner_principal_id: row.owner_principal_id,
        description: row.description,
        created_at: row.created_at,
    })
}

fn parse_gts_id(raw: &str) -> Result<GtsInstanceId, DomainError> {
    GtsInstanceId::try_new(raw)
        .map_err(|e| DomainError::Internal(format!("stored GTS id '{raw}' is malformed: {e}")))
}

pub struct Storage {
    db: Arc<DBProvider<toolkit_db::DbError>>,
    spec_manager: Arc<dyn SpecificationManager>,
    /// `None` until `EventBrokerModule::serve()` resolves it. `ClusterGear`
    /// (a `RunnableCapability`) only registers its backends into the
    /// `ClientHub` during the platform's *start* phase - which runs after
    /// *every* gear's `init()` (`host_runtime.rs`'s phase order: `init` ->
    /// ... -> REST -> start). `Storage` is built in `init()` (needed there
    /// for `spec_manager`/`db` wiring), so at that point
    /// `EventBrokerCluster::resolve()` would always fail - discovered by
    /// actually booting the standalone binary, not by any test (every
    /// existing test wires the cluster cache directly via
    /// `standalone_event_broker_cluster()`, bypassing the real gear
    /// lifecycle entirely). Set once via [`Self::set_cache`], called from
    /// `serve()` after the start phase has begun (so `cluster`'s own
    /// `start()`, which the topo-sorted dep order runs first, has already
    /// completed). Safe: every method that reads this is a request-path
    /// call, reachable only once the server is fully up.
    cache: OnceLock<ClusterCacheV1>,
    /// The ingest outbox pipeline - `None` until `EventBrokerModule::serve()`
    /// starts it (`Outbox::builder(..).start()` needs a running Tokio
    /// runtime and the leased handler's own dependencies, neither available
    /// yet at `Storage::new()` time in `init()`). Set once via
    /// [`Self::set_outbox`]. `check_and_enqueue` panics if called before
    /// it's set - matches `IngestService`'s own contract that no publish
    /// traffic reaches it before `serve()` has started (`host_runtime.rs`'s
    /// REST-phase-before-start-phase ordering).
    outbox: OnceLock<Arc<Outbox>>,
}

impl Storage {
    #[must_use]
    pub fn new(
        db: Arc<DBProvider<toolkit_db::DbError>>,
        spec_manager: Arc<dyn SpecificationManager>,
    ) -> Self {
        Self {
            db,
            spec_manager,
            cache: OnceLock::new(),
            outbox: OnceLock::new(),
        }
    }

    /// Wires the `subscription` namespace's `ClusterCacheV1` in once
    /// `EventBrokerModule::serve()` has resolved it.
    ///
    /// # Panics
    /// Panics if called more than once.
    pub fn set_cache(&self, cache: ClusterCacheV1) {
        assert!(
            self.cache.set(cache).is_ok(),
            "Storage::set_cache called twice"
        );
    }

    #[allow(clippy::expect_used)]
    fn cache(&self) -> &ClusterCacheV1 {
        self.cache
            .get()
            .expect("Storage subscription-namespace access before EventBrokerModule::serve() resolved the cluster cache")
    }

    /// Wires the ingest outbox pipeline in once `EventBrokerModule::serve()`
    /// has started it.
    ///
    /// # Panics
    /// Panics if called more than once.
    pub fn set_outbox(&self, outbox: Arc<Outbox>) {
        assert!(
            self.outbox.set(outbox).is_ok(),
            "Storage::set_outbox called twice"
        );
    }
}

#[async_trait]
impl ConsumerGroupRepo for Storage {
    async fn create_consumer_group(
        &self,
        group: ConsumerGroup,
    ) -> Result<ConsumerGroup, DomainError> {
        let conn = self.db.conn()?;
        let am = consumer_group::ActiveModel {
            id: Set(group.id.as_ref().to_owned()),
            kind: Set(kind_to_str(group.kind).to_owned()),
            tenant_id: Set(group.tenant_id),
            owner_principal_id: Set(group.owner_principal_id),
            description: Set(group.description.clone()),
            created_at: Set(group.created_at),
        };
        secure_insert::<consumer_group::Entity>(
            am,
            &AccessScope::for_tenant(group.tenant_id),
            &conn,
        )
        .await?;
        Ok(group)
    }

    async fn find_consumer_group(
        &self,
        id: &GtsInstanceId,
    ) -> Result<Option<ConsumerGroup>, DomainError> {
        let conn = self.db.conn()?;
        let row = consumer_group::Entity::find_by_id(id.as_ref().to_owned())
            .secure()
            .scope_with(&AccessScope::allow_all())
            .one(&conn)
            .await?;
        row.map(row_to_consumer_group).transpose()
    }

    /// **Known limitation, deliberately scoped (not silently dropped):**
    /// still fetches every row in one query rather than pushing
    /// `limit`/`cursor` to SQL. A correct pushdown implementation would also
    /// need to move `DeliveryServiceImpl::list_consumer_groups`'s
    /// anonymous-vs-named tenant-visibility partitioning
    /// (`eb-tenant-isolation-fix`) into the SQL `WHERE` clause, since that
    /// logic - not a plain tenant-scoped `AccessScope` - decides which rows
    /// a caller may see; that's a larger, separate piece of work than this
    /// change's storage-layer scope. What *did* change: this now queries a
    /// real SQLite table instead of an in-memory `HashMap`, resolving the
    /// "no persistence" half of the original gap.
    async fn list_consumer_groups(&self) -> Result<Vec<ConsumerGroup>, DomainError> {
        let conn = self.db.conn()?;
        let rows = consumer_group::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await?;
        rows.into_iter().map(row_to_consumer_group).collect()
    }

    async fn delete_consumer_group(&self, id: &GtsInstanceId) -> Result<(), DomainError> {
        let conn = self.db.conn()?;
        consumer_group::Entity::delete_many()
            .filter(consumer_group::Column::Id.eq(id.as_ref()))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(&conn)
            .await?;
        Ok(())
    }

    async fn has_active_members(&self, id: &GtsInstanceId) -> Result<bool, DomainError> {
        let keys = self.cache().scan_prefix(SUBSCRIPTION_KEY_PREFIX).await?;
        for key in keys {
            let Some(entry) = self.cache().get(&key).await? else {
                continue;
            };
            let subscription: Subscription = match serde_json::from_slice(&entry.value) {
                Ok(s) => s,
                Err(err) => {
                    tracing::warn!(%err, key, "failed to deserialize cached subscription");
                    continue;
                }
            };
            if &subscription.consumer_group == id {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

#[async_trait]
impl CursorRepo for Storage {
    async fn find_cursor(
        &self,
        consumer_group: &GtsInstanceId,
        topic: &GtsInstanceId,
        partition: i32,
    ) -> Result<Option<Cursor>, DomainError> {
        let topic_id = self.spec_manager.resolve_topic_id(topic).await?;
        let conn = self.db.conn()?;
        let row = cursor::Entity::find()
            .filter(cursor::Column::ConsumerGroup.eq(consumer_group.as_ref()))
            .filter(cursor::Column::TopicId.eq(topic_id))
            .filter(cursor::Column::Partition.eq(partition))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .one(&conn)
            .await?;
        Ok(row.map(|row| Cursor {
            topic: topic.clone(),
            consumer_group: consumer_group.clone(),
            partition: row.partition,
            offset: Sequence::assigned(row.offset),
        }))
    }

    async fn put_cursor(&self, cursor: &Cursor) -> Result<(), DomainError> {
        let topic_id = self.spec_manager.resolve_topic_id(&cursor.topic).await?;
        let conn = self.db.conn()?;

        let update_result = crate::infra::storage::entity::cursor::Entity::update_many()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .filter(
                Condition::all()
                    .add(
                        crate::infra::storage::entity::cursor::Column::ConsumerGroup
                            .eq(cursor.consumer_group.as_ref()),
                    )
                    .add(crate::infra::storage::entity::cursor::Column::TopicId.eq(topic_id))
                    .add(
                        crate::infra::storage::entity::cursor::Column::Partition
                            .eq(cursor.partition),
                    ),
            )
            .col_expr(
                crate::infra::storage::entity::cursor::Column::Offset,
                Expr::value(cursor.offset.as_i64()),
            )
            .col_expr(
                crate::infra::storage::entity::cursor::Column::UpdatedAt,
                Expr::value(Utc::now()),
            )
            .exec(&conn)
            .await?;

        if update_result.rows_affected > 0 {
            return Ok(());
        }

        // First write for this (consumer_group, topic, partition) - resolve
        // the owning consumer_group's tenant to denormalize onto the row
        // (decision log entry 28).
        let group = consumer_group::Entity::find_by_id(cursor.consumer_group.as_ref().to_owned())
            .secure()
            .scope_with(&AccessScope::allow_all())
            .one(&conn)
            .await?
            .ok_or_else(|| DomainError::NotFound {
                code: "ConsumerGroupNotFound",
                message: format!(
                    "consumer group '{}' is not registered",
                    cursor.consumer_group
                ),
                resource: cursor.consumer_group.to_string(),
            })?;

        let am = crate::infra::storage::entity::cursor::ActiveModel {
            consumer_group: Set(cursor.consumer_group.as_ref().to_owned()),
            topic_id: Set(topic_id),
            partition: Set(cursor.partition),
            tenant_id: Set(group.tenant_id),
            offset: Set(cursor.offset.as_i64()),
            updated_at: Set(Utc::now()),
        };
        secure_insert::<crate::infra::storage::entity::cursor::Entity>(
            am,
            &AccessScope::for_tenant(group.tenant_id),
            &conn,
        )
        .await?;
        Ok(())
    }
}

#[async_trait]
impl IdempotencyGuard for Storage {
    async fn check_and_enqueue(
        &self,
        request: PublishEnqueue,
    ) -> Result<ProducerIdempotencyOutcome, DomainError> {
        // A request must never be able to panic the process, and this one can
        // genuinely arrive before the pipeline exists: the REST listener is up
        // (and `/healthz`, a liveness probe, answers 200) before the gear's
        // start phase runs, and an instance deployed without the ingest role
        // never starts a pipeline at all. Both are retry-or-route-elsewhere
        // conditions for the caller, so they are reported as an unavailable
        // storage path rather than an internal failure.
        let outbox = Arc::clone(self.outbox.get().ok_or_else(|| {
            DomainError::StorageUnavailable(
                "the ingest outbox pipeline is not running yet".to_owned(),
            )
        })?);
        let PublishEnqueue {
            chain,
            topic_partition,
            payload,
            payload_type,
        } = request;
        // Every event of one topic partition has to reach the same outbox
        // partition to keep its relative order, and the topic's partition
        // count is unrelated to the outbox's - see `outbox_partition_for`.
        let partition_for_outbox = crate::domain::outbox::outbox_partition_for(topic_partition);

        let outcome = self
            .db
            .transaction(move |tx| {
                Box::pin(async move {
                    let outcome = match &chain {
                        None => ProducerIdempotencyOutcome::Accept,
                        Some(c) => {
                            let existing = producer_sequence::Entity::find()
                                .filter(producer_sequence::Column::ProducerId.eq(c.producer_id))
                                .filter(producer_sequence::Column::Topic.eq(c.topic.as_ref()))
                                .filter(producer_sequence::Column::Partition.eq(c.partition))
                                .secure()
                                .scope_with(&AccessScope::allow_all())
                                .one(tx)
                                .await
                                .map_err(|e| db_err_from_scope(&e))?;

                            let outcome = match existing {
                                None => {
                                    let producer_row = producer::Entity::find_by_id(c.producer_id)
                                        .secure()
                                        .scope_with(&AccessScope::allow_all())
                                        .one(tx)
                                        .await
                                        .map_err(|e| db_err_from_scope(&e))?
                                        .ok_or_else(|| {
                                            toolkit_db::DbError::Other(anyhow::anyhow!(
                                                "producer '{}' is not registered",
                                                c.producer_id
                                            ))
                                        })?;
                                    let am = producer_sequence::ActiveModel {
                                        producer_id: Set(c.producer_id),
                                        topic: Set(c.topic.as_ref().to_owned()),
                                        partition: Set(c.partition),
                                        tenant_id: Set(producer_row.tenant_id),
                                        last_sequence: Set(c.sequence),
                                        updated_at: Set(Utc::now()),
                                    };
                                    secure_insert::<producer_sequence::Entity>(
                                        am,
                                        &AccessScope::for_tenant(producer_row.tenant_id),
                                        tx,
                                    )
                                    .await
                                    .map_err(|e| db_err_from_scope(&e))?;
                                    ProducerIdempotencyOutcome::Accept
                                }
                                Some(row)
                                    if c.sequence == row.last_sequence + 1
                                        && c.previous == row.last_sequence =>
                                {
                                    producer_sequence::Entity::update_many()
                                        .secure()
                                        .scope_with(&AccessScope::allow_all())
                                        .filter(
                                            Condition::all()
                                                .add(
                                                    producer_sequence::Column::ProducerId
                                                        .eq(c.producer_id),
                                                )
                                                .add(
                                                    producer_sequence::Column::Topic
                                                        .eq(c.topic.as_ref()),
                                                )
                                                .add(
                                                    producer_sequence::Column::Partition
                                                        .eq(c.partition),
                                                ),
                                        )
                                        .col_expr(
                                            producer_sequence::Column::LastSequence,
                                            Expr::value(c.sequence),
                                        )
                                        .col_expr(
                                            producer_sequence::Column::UpdatedAt,
                                            Expr::value(Utc::now()),
                                        )
                                        .exec(tx)
                                        .await
                                        .map_err(|e| db_err_from_scope(&e))?;
                                    ProducerIdempotencyOutcome::Accept
                                }
                                Some(row) if c.sequence <= row.last_sequence => {
                                    ProducerIdempotencyOutcome::DuplicateIgnore
                                }
                                Some(row) => ProducerIdempotencyOutcome::SequenceViolation {
                                    last_sequence: row.last_sequence,
                                },
                            };
                            outcome
                        }
                    };

                    if matches!(outcome, ProducerIdempotencyOutcome::Accept) {
                        outbox
                            .enqueue(
                                tx,
                                crate::domain::outbox::INGEST_QUEUE_NAME,
                                partition_for_outbox,
                                payload,
                                &payload_type,
                            )
                            .await
                            .map_err(|e| {
                                toolkit_db::DbError::Other(anyhow::anyhow!("outbox enqueue: {e}"))
                            })?;
                    }

                    Ok(outcome)
                })
            })
            .await?;
        Ok(outcome)
    }
}

#[async_trait]
impl DeliveryNotifier for Storage {
    async fn wait_for_notification(&self, timeout: std::time::Duration) {
        // One shared deadline for both steps (subscribing, then waiting for
        // the first event) - `wait_for_notification`'s contract is "wait up
        // to `timeout` total", not up to `timeout` per step.
        let deadline = tokio::time::Instant::now() + timeout;
        let mut watch =
            match tokio::time::timeout_at(deadline, self.cache().watch_prefix(NOTIFICATION_PREFIX))
                .await
            {
                Ok(Ok(watch)) => watch,
                Ok(Err(err)) => {
                    tracing::warn!(
                        ?err,
                        "notification watch_prefix failed; falling back to timeout"
                    );
                    return;
                }
                Err(_elapsed) => return,
            };
        // A `Lagged`/`Reset` event still just means "something changed, go
        // re-check", same as a real `Event`, and an elapsed deadline just
        // means "nothing changed in time" - either way there's nothing
        // further to do with the result.
        match tokio::time::timeout_at(deadline, watch.recv()).await {
            Ok(_) | Err(_) => {}
        }
    }
}

#[async_trait]
impl ProducerRegistry for Storage {
    async fn register(
        &self,
        owner: Uuid,
        tenant_id: Uuid,
        mode: ProducerMode,
        client_agent: String,
    ) -> Result<ProducerRegistration, DomainError> {
        let conn = self.db.conn()?;
        let id = Uuid::new_v4();
        let mode_str = match mode {
            ProducerMode::Chained => "chained",
            ProducerMode::Monotonic => "monotonic",
        };
        let am = producer::ActiveModel {
            id: Set(id),
            tenant_id: Set(tenant_id),
            owner_id: Set(owner),
            mode: Set(mode_str.to_owned()),
            client_agent: Set(client_agent.clone()),
            created_at: Set(Utc::now()),
        };
        secure_insert::<producer::Entity>(am, &AccessScope::for_tenant(tenant_id), &conn).await?;
        Ok(ProducerRegistration {
            id,
            mode,
            client_agent,
        })
    }

    async fn owner(&self, producer_id: Uuid) -> Result<Option<Uuid>, DomainError> {
        let conn = self.db.conn()?;
        let row = producer::Entity::find_by_id(producer_id)
            .secure()
            .scope_with(&AccessScope::allow_all())
            .one(&conn)
            .await?;
        Ok(row.map(|r| r.owner_id))
    }

    async fn cursors(&self, producer_id: Uuid) -> Result<Option<ProducerCursors>, DomainError> {
        let conn = self.db.conn()?;
        let Some(producer_row) = producer::Entity::find_by_id(producer_id)
            .secure()
            .scope_with(&AccessScope::allow_all())
            .one(&conn)
            .await?
        else {
            return Ok(None);
        };

        let sequence_rows = producer_sequence::Entity::find()
            .filter(producer_sequence::Column::ProducerId.eq(producer_id))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await?;

        let mut by_topic: std::collections::HashMap<String, Vec<ProducerPartitionCursor>> =
            std::collections::HashMap::new();
        for row in sequence_rows {
            by_topic
                .entry(row.topic)
                .or_default()
                .push(ProducerPartitionCursor {
                    partition: row.partition,
                    last_sequence: row.last_sequence,
                });
        }

        Ok(Some(ProducerCursors {
            producer_id,
            client_agent: producer_row.client_agent,
            topics: by_topic
                .into_iter()
                .map(|(topic, partitions)| ProducerTopicCursors { topic, partitions })
                .collect(),
        }))
    }

    async fn reset(
        &self,
        producer_id: Uuid,
        scope: &ProducerResetScope,
    ) -> Result<(), DomainError> {
        let conn = self.db.conn()?;
        let mut filter =
            Condition::all().add(producer_sequence::Column::ProducerId.eq(producer_id));
        if let ProducerResetScope::TopicPartition { topic, partition } = scope {
            filter = filter
                .add(producer_sequence::Column::Topic.eq(topic.as_str()))
                .add(producer_sequence::Column::Partition.eq(*partition));
        }
        producer_sequence::Entity::delete_many()
            .filter(filter)
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(&conn)
            .await?;
        Ok(())
    }
}

#[async_trait]
impl SubscriptionRepo for Storage {
    async fn find_subscription(&self, id: Uuid) -> Result<Option<Subscription>, DomainError> {
        let Some(entry) = self.cache().get(&subscription_key(id)).await? else {
            return Ok(None);
        };
        let subscription: Subscription = serde_json::from_slice(&entry.value).map_err(|e| {
            DomainError::Internal(format!("failed to deserialize cached subscription: {e}"))
        })?;
        Ok(Some(subscription))
    }

    /// Full scan (`scan_prefix` + per-key `get`), matching
    /// `InMemoryDomainRepo`'s own full-`Vec`-scan complexity today - not a
    /// regression, since `Subscription` was already an unindexed in-memory
    /// collection before this change (eb-single-process-implementation
    /// task 4.2).
    async fn list_subscriptions(&self) -> Result<Vec<Subscription>, DomainError> {
        let keys = self.cache().scan_prefix(SUBSCRIPTION_KEY_PREFIX).await?;
        let mut subscriptions = Vec::with_capacity(keys.len());
        for key in keys {
            let Some(entry) = self.cache().get(&key).await? else {
                continue;
            };
            match serde_json::from_slice::<Subscription>(&entry.value) {
                Ok(subscription) => subscriptions.push(subscription),
                Err(err) => {
                    tracing::warn!(%err, key, "failed to deserialize cached subscription");
                }
            }
        }
        Ok(subscriptions)
    }

    async fn put_subscription(&self, subscription: &Subscription) -> Result<(), DomainError> {
        let value = serde_json::to_vec(subscription)
            .map_err(|e| DomainError::Internal(format!("failed to serialize subscription: {e}")))?;
        // TTL matches the subscription's own session_timeout (design.md D2) -
        // an expired-but-not-yet-reaped subscription simply falls out of the
        // cache on its own; the reaper worker's job is idempotency-key
        // cleanup and any earlier explicit-delete path, not this TTL.
        self.cache()
            .put(PutRequest {
                key: &subscription_key(subscription.id),
                value: &value,
                ttl: Ttl::Of(subscription.session_timeout),
            })
            .await?;
        Ok(())
    }

    async fn delete_subscription(&self, id: Uuid) -> Result<(), DomainError> {
        self.cache().delete(&subscription_key(id)).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage, Partitions};
    use types_registry_sdk::testing::{MockTypesRegistryClient, make_test_instance};

    use super::*;
    use crate::domain::idempotency::ProducerChainCheck;
    use crate::domain::model::{BarrierMode, ConsumerGroupKind, Interest, TenantTraversalDepth};
    use crate::infra::specification::TypesRegistrySpecificationManager;
    use crate::infra::storage::migrations::Migrator;

    const TOPIC_ID: &str = "gts.cf.core.events.topic.v1~example.eb.storage.topic.v1";

    /// A `LeasedMessageHandler` that always acknowledges - `Storage`'s own
    /// unit tests exercise `check_and_enqueue`'s transactional
    /// producer-state + outbox-insert behavior, not the drain side (that's
    /// `infra::outbox::IngestOutboxHandler`'s own test module).
    struct NoopLeasedHandler;

    #[async_trait]
    impl LeasedMessageHandler for NoopLeasedHandler {
        async fn handle(&self, _msg: &OutboxMessage) -> MessageResult {
            MessageResult::Ok
        }
    }

    /// A fresh temp-file `SQLite` database, and the DSN that reaches it.
    ///
    /// The DSN comes back so a test can open its own second connection: the
    /// outbox's bookkeeping tables are not reachable through `DBProvider`
    /// (`toolkit-db` deliberately hands out no raw `SeaORM` executor), and a
    /// test that needs to see where a row was routed has to read them.
    async fn test_db() -> (Arc<DBProvider<toolkit_db::DbError>>, String) {
        let mut path = std::env::temp_dir();
        path.push(format!("cf-eb-storage-test-{}.db", Uuid::now_v7().simple()));
        let mut file = path.to_string_lossy().replace('\\', "/");
        if !file.starts_with('/') {
            file.insert(0, '/');
        }
        let dsn = format!("sqlite://{file}?mode=rwc");
        let opts = toolkit_db::ConnectOpts {
            max_conns: Some(1),
            min_conns: Some(1),
            ..Default::default()
        };
        let db = toolkit_db::connect_db(&dsn, opts)
            .await
            .expect("connect sqlite");
        toolkit_db::migration_runner::run_migrations_for_testing(
            &db,
            <Migrator as sea_orm_migration::MigratorTrait>::migrations(),
        )
        .await
        .expect("migrations");
        (Arc::new(DBProvider::new(db)), dsn)
    }

    /// A `Storage` wired against a fresh temp-file SQLite DB and a real
    /// (`standalone`-provider-backed) `ClusterCacheV1`, with one topic
    /// (`TOPIC_ID`) already bulk-loaded into `SpecificationManager` so
    /// `CursorRepo` tests can resolve it.
    async fn test_storage() -> Storage {
        test_storage_with_db().await.0
    }

    /// As [`test_storage`], but hands back the `DBProvider` too, so a test can
    /// read the outbox's own bookkeeping tables directly.
    async fn test_storage_with_db() -> (Storage, String) {
        let (db, dsn) = test_db().await;
        let instance = make_test_instance(
            TOPIC_ID,
            serde_json::json!({
                "id": TOPIC_ID,
                "description": "a topic this test keeps cursors for",
            }),
        );
        let client: Arc<dyn types_registry_sdk::TypesRegistryClient> =
            Arc::new(MockTypesRegistryClient::new().with_instances(vec![instance]));
        crate::infra::specification::bulk_load(
            &client,
            &db,
            &crate::test_support::StaticTypesRegistry::empty_config(),
        )
        .await
        .expect("bulk_load");
        let spec_manager = TypesRegistrySpecificationManager::new(Arc::clone(&db));

        let (hub, _cluster) = crate::test_support::standalone_event_broker_cluster().await;
        let cache = crate::domain::cluster::EventBrokerCluster::resolve(&hub)
            .await
            .expect("cluster resolves")
            .cache;

        let storage = Storage::new(Arc::clone(&db), Arc::new(spec_manager));
        storage.set_cache(cache);

        // `check_and_enqueue` needs a started outbox pipeline to insert
        // into - a no-op drain handler is enough for these tests, which
        // exercise the enqueue transaction, not the drain side. The
        // returned handle is intentionally dropped without `.stop()`: per
        // `OutboxHandle`'s own doc comment, dropping it cancels the
        // pipeline's background workers immediately - but `enqueue()` is a
        // plain DB insert that doesn't depend on those workers running, so
        // these tests (which never assert on a row actually draining) are
        // unaffected. Contrast `test_support::harness`'s `EventBrokerHarness`,
        // which keeps its handle alive for its whole lifetime because its
        // tests do rely on real draining.
        let handle = Outbox::builder(db.db())
            .queue(crate::domain::outbox::INGEST_QUEUE_NAME, Partitions::of(4))
            .leased(NoopLeasedHandler)
            .start()
            .await
            .expect("outbox start");
        storage.set_outbox(Arc::clone(handle.outbox()));

        (storage, dsn)
    }

    /// How many rows each registered ingest outbox partition holds, ascending
    /// by partition. A partition at `0` never received anything.
    ///
    /// Read from the staging table rather than from
    /// `toolkit_outbox_partitions.sequence`, because these tests deliberately
    /// drop the pipeline handle (see [`test_storage_with_db`]) and so the
    /// sequencer that fills that column never runs. Enqueue's own routing
    /// decision is already final here: the row names its partition the moment
    /// it is staged.
    async fn outbox_partition_row_counts(dsn: &str) -> Vec<(i32, i64)> {
        use sea_orm::{ConnectionTrait, Statement};

        let conn = sea_orm::Database::connect(dsn).await.expect("connect");
        let rows = conn
            .query_all_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Sqlite,
                "SELECT p.partition AS partition, COUNT(i.id) AS rows_staged \
                 FROM toolkit_outbox_partitions AS p \
                 LEFT JOIN toolkit_outbox_incoming AS i ON i.partition_id = p.id \
                 WHERE p.queue = ? GROUP BY p.partition ORDER BY p.partition",
                [crate::domain::outbox::INGEST_QUEUE_NAME.into()],
            ))
            .await
            .expect("read outbox partitions");
        rows.into_iter()
            .map(|row| {
                (
                    row.try_get::<i32>("", "partition").expect("partition"),
                    row.try_get::<i64>("", "rows_staged").expect("rows_staged"),
                )
            })
            .collect()
    }

    /// Stateless publishes - the common case, no producer `meta` at all - used
    /// to be enqueued onto outbox partition `0` unconditionally, leaving every
    /// other sequencer/processor slot permanently idle.
    #[tokio::test]
    async fn stateless_publishes_reach_every_outbox_partition() {
        let (storage, dsn) = test_storage_with_db().await;

        // One publish per topic partition of an eight-partition topic. Eight
        // is deliberately more than the outbox has, so this also covers the
        // partitions that used to address slots that do not exist.
        for topic_partition in 0..8 {
            let outcome = storage
                .check_and_enqueue(
                    PublishEnqueue::builder(
                        topic_partition,
                        b"payload".to_vec(),
                        "application/json",
                    )
                    .build(),
                )
                .await
                .expect("check_and_enqueue must succeed");
            assert_eq!(outcome, ProducerIdempotencyOutcome::Accept);
        }

        assert_eq!(
            outbox_partition_row_counts(&dsn).await,
            vec![(0, 2), (1, 2), (2, 2), (3, 2)],
            "eight topic partitions over a four-partition outbox must land two \
             apiece, not eight on partition 0"
        );
    }

    /// A topic with more partitions than the outbox has used to hand
    /// `toolkit-db` a partition index it had never registered, which failed
    /// the whole enqueue transaction and rejected the publish.
    #[tokio::test]
    async fn chained_publish_beyond_the_outbox_partition_count_is_accepted() {
        let (storage, dsn) = test_storage_with_db().await;
        let owner = Uuid::new_v4();
        let registration = storage
            .register(
                owner,
                Uuid::new_v4(),
                ProducerMode::Chained,
                "agent".to_owned(),
            )
            .await
            .expect("register must succeed");
        let topic = GtsInstanceId::try_new(TOPIC_ID).unwrap();

        // Partition 7 of an eight-partition topic: past the outbox's four.
        let outcome = storage
            .check_and_enqueue(
                PublishEnqueue::builder(7, b"payload".to_vec(), "application/json")
                    .chain(ProducerChainCheck {
                        producer_id: registration.id,
                        topic,
                        partition: 7,
                        previous: 0,
                        sequence: 1,
                    })
                    .build(),
            )
            .await
            .expect("a chained publish past the outbox partition count must be accepted");
        assert_eq!(outcome, ProducerIdempotencyOutcome::Accept);

        assert_eq!(
            outbox_partition_row_counts(&dsn).await,
            vec![(0, 0), (1, 0), (2, 0), (3, 1)],
            "topic partition 7 must land on outbox partition 3"
        );
    }

    /// A publish can genuinely arrive before the outbox pipeline exists - the
    /// REST listener is up before the gear's start phase runs - and used to
    /// panic the request task. It must be a retryable answer instead.
    #[tokio::test]
    async fn publish_before_the_pipeline_starts_is_reported_as_unavailable() {
        let (db, _dsn) = test_db().await;
        let spec_manager = TypesRegistrySpecificationManager::new(Arc::clone(&db));
        // No `set_outbox`: exactly the state `Storage` is in between `init()`
        // and `serve()`.
        let storage = Storage::new(db, Arc::new(spec_manager));

        let err = storage
            .check_and_enqueue(
                PublishEnqueue::builder(0, b"payload".to_vec(), "application/json").build(),
            )
            .await
            .expect_err("a publish before the pipeline starts must not succeed");

        assert!(
            matches!(&err, DomainError::StorageUnavailable(detail)
                if detail == "the ingest outbox pipeline is not running yet"),
            "expected a retryable StorageUnavailable, got {err:?}"
        );
    }

    fn test_consumer_group(id: &str, tenant_id: Uuid) -> ConsumerGroup {
        ConsumerGroup {
            id: GtsInstanceId::try_new(id).unwrap(),
            kind: ConsumerGroupKind::Anonymous,
            tenant_id,
            owner_principal_id: Uuid::new_v4(),
            description: None,
            created_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn consumer_group_create_find_list_delete_round_trip() {
        let storage = test_storage().await;
        let tenant_id = Uuid::new_v4();
        let group = test_consumer_group(
            "gts.cf.core.events.consumer_group.v1~example.eb.storage.cg1.v1",
            tenant_id,
        );

        let created = storage
            .create_consumer_group(group.clone())
            .await
            .expect("create must succeed");
        assert_eq!(created.id, group.id);

        let found = storage
            .find_consumer_group(&group.id)
            .await
            .expect("find must succeed")
            .expect("group must exist");
        assert_eq!(found.tenant_id, tenant_id);
        assert_eq!(found.kind, ConsumerGroupKind::Anonymous);

        let listed = storage
            .list_consumer_groups()
            .await
            .expect("list must succeed");
        assert!(listed.iter().any(|g| g.id == group.id));

        storage
            .delete_consumer_group(&group.id)
            .await
            .expect("delete must succeed");
        assert!(
            storage
                .find_consumer_group(&group.id)
                .await
                .expect("find after delete must succeed")
                .is_none()
        );
    }

    #[tokio::test]
    async fn has_active_members_reflects_a_live_subscription() {
        let storage = test_storage().await;
        let tenant_id = Uuid::new_v4();
        let group = test_consumer_group(
            "gts.cf.core.events.consumer_group.v1~example.eb.storage.cg2.v1",
            tenant_id,
        );
        storage
            .create_consumer_group(group.clone())
            .await
            .expect("create must succeed");

        assert!(
            !storage
                .has_active_members(&group.id)
                .await
                .expect("has_active_members must succeed"),
            "no subscription exists yet"
        );

        let subscription = Subscription {
            id: Uuid::new_v4(),
            tenant_id,
            consumer_group: group.id.clone(),
            client_agent: "test-agent".to_owned(),
            interests: vec![Interest {
                topic: GtsInstanceId::try_new(TOPIC_ID).unwrap(),
                tenant_id,
                depth: TenantTraversalDepth::CurrentTenant,
                barrier_mode: BarrierMode::Respect,
                types: vec![],
                filter: None,
            }],
            topics: vec![GtsInstanceId::try_new(TOPIC_ID).unwrap()],
            assigned: vec![],
            topology_version: 1,
            session_timeout: std::time::Duration::from_secs(30),
            last_seen_at: Utc::now(),
            expires_at: Utc::now() + chrono::Duration::seconds(30),
        };
        storage
            .put_subscription(&subscription)
            .await
            .expect("put_subscription must succeed");

        assert!(
            storage
                .has_active_members(&group.id)
                .await
                .expect("has_active_members must succeed"),
            "a live subscription now belongs to this group"
        );
    }

    #[tokio::test]
    async fn cursor_find_put_round_trip_denormalizes_tenant() {
        let storage = test_storage().await;
        let tenant_id = Uuid::new_v4();
        let group = test_consumer_group(
            "gts.cf.core.events.consumer_group.v1~example.eb.storage.cg3.v1",
            tenant_id,
        );
        storage
            .create_consumer_group(group.clone())
            .await
            .expect("create must succeed");

        let topic = GtsInstanceId::try_new(TOPIC_ID).unwrap();
        assert!(
            storage
                .find_cursor(&group.id, &topic, 0)
                .await
                .expect("find_cursor must succeed")
                .is_none()
        );

        let cursor = Cursor {
            topic: topic.clone(),
            consumer_group: group.id.clone(),
            partition: 0,
            offset: Sequence::assigned(42),
        };
        storage
            .put_cursor(&cursor)
            .await
            .expect("put_cursor must succeed");

        let found = storage
            .find_cursor(&group.id, &topic, 0)
            .await
            .expect("find_cursor must succeed")
            .expect("cursor must exist");
        assert_eq!(found.offset, Sequence::assigned(42));

        // Update in place - same (consumer_group, topic, partition) key.
        let updated = Cursor {
            offset: Sequence::assigned(99),
            ..cursor
        };
        storage
            .put_cursor(&updated)
            .await
            .expect("put_cursor update must succeed");
        let found = storage
            .find_cursor(&group.id, &topic, 0)
            .await
            .expect("find_cursor must succeed")
            .expect("cursor must exist");
        assert_eq!(
            found.offset,
            Sequence::assigned(99),
            "second put_cursor must update, not duplicate"
        );
    }

    #[tokio::test]
    async fn subscription_find_put_delete_round_trip() {
        let storage = test_storage().await;
        let id = Uuid::new_v4();
        assert!(
            storage
                .find_subscription(id)
                .await
                .expect("find must succeed")
                .is_none()
        );

        let subscription = Subscription {
            id,
            tenant_id: Uuid::new_v4(),
            consumer_group: GtsInstanceId::try_new(
                "gts.cf.core.events.consumer_group.v1~example.eb.storage.cg4.v1",
            )
            .unwrap(),
            client_agent: "test-agent".to_owned(),
            interests: vec![],
            topics: vec![],
            assigned: vec![],
            topology_version: 1,
            session_timeout: std::time::Duration::from_secs(30),
            last_seen_at: Utc::now(),
            expires_at: Utc::now() + chrono::Duration::seconds(30),
        };
        storage
            .put_subscription(&subscription)
            .await
            .expect("put must succeed");

        let found = storage
            .find_subscription(id)
            .await
            .expect("find must succeed")
            .expect("subscription must exist");
        assert_eq!(found.client_agent, "test-agent");

        storage
            .delete_subscription(id)
            .await
            .expect("delete must succeed");
        assert!(
            storage
                .find_subscription(id)
                .await
                .expect("find after delete must succeed")
                .is_none()
        );
    }

    #[tokio::test]
    async fn producer_register_and_idempotency_round_trip() {
        let storage = test_storage().await;
        let owner = Uuid::new_v4();
        let tenant_id = Uuid::new_v4();

        let registration = storage
            .register(owner, tenant_id, ProducerMode::Chained, "agent".to_owned())
            .await
            .expect("register must succeed");
        assert_eq!(
            storage
                .owner(registration.id)
                .await
                .expect("owner lookup must succeed"),
            Some(owner)
        );

        let topic = GtsInstanceId::try_new(TOPIC_ID).unwrap();
        let enqueue = |previous, sequence| {
            PublishEnqueue::builder(0, b"payload".to_vec(), "application/json")
                .chain(ProducerChainCheck {
                    producer_id: registration.id,
                    topic: topic.clone(),
                    partition: 0,
                    previous,
                    sequence,
                })
                .build()
        };

        // First accept: no prior state, any (previous, sequence) is accepted.
        let outcome = storage
            .check_and_enqueue(enqueue(0, 1))
            .await
            .expect("check_and_enqueue must succeed");
        assert_eq!(outcome, ProducerIdempotencyOutcome::Accept);

        // Retry of the same call: duplicate, ignored.
        let outcome = storage
            .check_and_enqueue(enqueue(0, 1))
            .await
            .expect("check_and_enqueue must succeed");
        assert_eq!(outcome, ProducerIdempotencyOutcome::DuplicateIgnore);

        // Correct next step in the chain: accepted.
        let outcome = storage
            .check_and_enqueue(enqueue(1, 2))
            .await
            .expect("check_and_enqueue must succeed");
        assert_eq!(outcome, ProducerIdempotencyOutcome::Accept);

        // A gap in the chain: sequence violation, carrying the last accepted
        // sequence.
        let outcome = storage
            .check_and_enqueue(enqueue(2, 10))
            .await
            .expect("check_and_enqueue must succeed");
        assert_eq!(
            outcome,
            ProducerIdempotencyOutcome::SequenceViolation { last_sequence: 2 }
        );

        let cursors = storage
            .cursors(registration.id)
            .await
            .expect("cursors lookup must succeed")
            .expect("producer must exist");
        assert_eq!(cursors.topics.len(), 1);
        assert_eq!(cursors.topics[0].partitions[0].last_sequence, 2);

        storage
            .reset(registration.id, &ProducerResetScope::All)
            .await
            .expect("reset must succeed");
        let cursors = storage
            .cursors(registration.id)
            .await
            .expect("cursors lookup must succeed")
            .expect("producer must still exist after reset");
        assert!(
            cursors.topics.is_empty(),
            "reset must clear all sequence state"
        );
    }
}
