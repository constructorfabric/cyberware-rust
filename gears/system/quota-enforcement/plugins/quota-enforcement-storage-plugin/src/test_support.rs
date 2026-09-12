//! Test doubles and harness recipes shared by the plugin's test modules.

#![allow(clippy::expect_used)]

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use gts::GtsTypeId;
use quota_enforcement_sdk::testing::quota_draft;
use quota_enforcement_sdk::{
    ActiveQuotaCounts, DeactivateOutcome, EventId, NotificationEvent, NotificationEventKind,
    PageRequest, PageResult, ProjectionBinding, Quota, QuotaDraft, QuotaFilter, QuotaId,
    QuotaPatch, SubjectRef, TenantId,
};
use sea_orm::EntityTrait;
use sea_orm_migration::MigratorTrait;
use serde_json::json;
use time::OffsetDateTime;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::outbox::{OutboxHandle, OutboxMessageId};
use toolkit_db::secure::{DBRunner, SecureEntityExt};
use toolkit_db::{ConnectOpts, Db, connect_db};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::ports::{Actor, QuotaStore, StoreError};
use crate::infra::outbox::{EnqueueError, NotificationEnqueuer, QeOutbox, start_outbox};
use crate::infra::storage::Migrator;

/// The `llm_gateway` user projection the gear's fixtures use.
pub const USER_PROJECTION: &str = "gts.cf.core.qe.subj.v1~cf.genai.llm_gateway.user.v1~";
/// A metric instance id.
pub const METRIC_TOKENS: &str = "gts.cf.qe.metric.type.v1~cf.qe.metric.ai_tokens_input.v1";
/// Another metric instance id.
pub const METRIC_REQUESTS: &str = "gts.cf.qe.metric.type.v1~cf.qe.metric.ai_requests.v1";

/// One in-memory `SQLite` database with every plugin migration applied. One
/// connection: a second connection would be a second database.
pub async fn test_db() -> Db {
    let opts = ConnectOpts {
        max_conns: Some(1),
        min_conns: Some(1),
        ..ConnectOpts::default()
    };
    let db = connect_db("sqlite::memory:", opts)
        .await
        .expect("connect in-memory sqlite");
    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("apply storage migrations");
    db
}

/// The outbox pipeline on `db` with the notification queue registered, and
/// a bound handle over it. Stop the handle at the end of the test.
pub async fn bound_outbox(db: &Db) -> (OutboxHandle, Arc<QeOutbox>) {
    let handle = start_outbox(db.clone()).await.expect("start outbox");
    let outbox = Arc::new(QeOutbox::new());
    outbox.bind(Arc::clone(handle.outbox())).expect("bind once");
    (handle, outbox)
}

/// The test tenant.
pub fn tenant() -> TenantId {
    TenantId::new(Uuid::from_u128(0x00ac_ce55))
}

/// Another tenant.
pub fn other_tenant() -> TenantId {
    TenantId::new(Uuid::from_u128(0x0bad))
}

/// A scope covering exactly `tenant`.
pub fn scope_for(tenant: TenantId) -> AccessScope {
    AccessScope::for_tenant(tenant.as_uuid())
}

/// The test actor.
pub fn actor() -> Actor {
    Actor {
        subject_id: Uuid::from_u128(0x5eed),
        subject_type: Some("user".to_owned()),
    }
}

/// A user subject.
pub fn user(id: &str) -> SubjectRef {
    SubjectRef {
        projection_type: GtsTypeId::try_new(USER_PROJECTION).expect("type id"),
        subject_id: id.to_owned(),
    }
}

/// A consumption draft for `tenant` on the tokens metric.
pub fn draft(tenant: TenantId, subject_id: &str, cap: Option<u64>) -> QuotaDraft {
    let mut draft = quota_draft(user(subject_id), cap);
    draft.tenant_id = tenant;
    draft.metric = quota_enforcement_sdk::MetricId::parse(METRIC_TOKENS).expect("metric");
    draft
}

/// A `quota-changed` event for `tenant` with no Quota id yet.
pub fn quota_changed(tenant: TenantId) -> NotificationEvent {
    NotificationEvent {
        event_id: EventId::generate(),
        kind: NotificationEventKind::QuotaChanged,
        tenant_id: tenant,
        quota_id: None,
        policy_id: None,
        subject: None,
        payload: json!({ "change_kind": "created" }),
        emitted_at: OffsetDateTime::now_utc(),
    }
}

/// Probe over the outbox body table: what was enqueued.
mod outbox_body {
    use sea_orm::entity::prelude::*;
    use toolkit_db_macros::Scopable;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "qe_outbox_body")]
    #[secure(no_tenant, no_resource, no_owner, no_type)]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        pub payload: Vec<u8>,
        pub payload_type: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl sea_orm::ActiveModelBehavior for ActiveModel {}
}

/// One enqueued message.
#[derive(Debug, Clone, PartialEq, Eq, sea_orm::FromQueryResult)]
pub struct EnqueuedMessage {
    pub payload: Vec<u8>,
    pub payload_type: String,
}

/// Every message in the outbox body table, oldest first.
pub async fn enqueued_messages(db: &Db) -> Vec<EnqueuedMessage> {
    use sea_orm::{QueryOrder, QuerySelect};
    let conn = db.conn().expect("connection");
    outbox_body::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .project_all(&conn, |q| {
            q.select_only()
                .column(outbox_body::Column::Payload)
                .column(outbox_body::Column::PayloadType)
                .order_by_asc(outbox_body::Column::Id)
                .into_model::<EnqueuedMessage>()
        })
        .await
        .expect("read outbox body")
}

/// Rows of a scopable entity under `allow_all`.
pub async fn count_rows<E: EntityTrait + toolkit_db::secure::ScopableEntity>(db: &Db) -> usize {
    let conn = db.conn().expect("connection");
    E::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&conn)
        .await
        .expect("read rows")
        .len()
}

/// An enqueuer that fails every call, for atomicity tests.
pub struct FailingEnqueuer;

#[async_trait]
impl NotificationEnqueuer for FailingEnqueuer {
    async fn enqueue_all(
        &self,
        _runner: &(dyn DBRunner + Sync),
        _events: &[NotificationEvent],
    ) -> Result<Vec<OutboxMessageId>, EnqueueError> {
        Err(EnqueueError::Serialize("injected".to_owned()))
    }
}

/// A Quota store double that answers with one configured error, or with
/// empty successes, and records what it was asked.
#[derive(Default)]
pub struct FakeQuotaStore {
    fail: Mutex<Option<StoreError>>,
    actors: Mutex<Vec<Actor>>,
}

impl FakeQuotaStore {
    /// A store that fails every call with `err`.
    pub fn failing(err: StoreError) -> Self {
        Self {
            fail: Mutex::new(Some(err)),
            actors: Mutex::new(Vec::new()),
        }
    }

    /// The actors of every mutation call.
    pub fn actors(&self) -> Vec<Actor> {
        self.actors.lock().expect("lock").clone()
    }

    fn check(&self, actor: Option<&Actor>) -> Result<(), StoreError> {
        if let Some(actor) = actor {
            self.actors.lock().expect("lock").push(actor.clone());
        }
        match self.fail.lock().expect("lock").clone() {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}

#[async_trait]
impl QuotaStore for FakeQuotaStore {
    async fn create_quota(
        &self,
        actor: &Actor,
        _scope: &AccessScope,
        _draft: QuotaDraft,
        _events: &[NotificationEvent],
    ) -> Result<QuotaId, StoreError> {
        self.check(Some(actor))?;
        Ok(QuotaId::generate())
    }

    async fn update_quota(
        &self,
        actor: &Actor,
        _scope: &AccessScope,
        quota_id: QuotaId,
        _patch: QuotaPatch,
        _events: &[NotificationEvent],
    ) -> Result<Quota, StoreError> {
        self.check(Some(actor))?;
        Err(StoreError::QuotaNotFound { id: quota_id })
    }

    async fn deactivate_quota(
        &self,
        actor: &Actor,
        _scope: &AccessScope,
        _quota_id: QuotaId,
        _events: &[NotificationEvent],
    ) -> Result<DeactivateOutcome, StoreError> {
        self.check(Some(actor))?;
        Ok(DeactivateOutcome::default())
    }

    async fn read_quotas(
        &self,
        _scope: &AccessScope,
        _filter: QuotaFilter,
        _page: PageRequest,
    ) -> Result<PageResult<Quota>, StoreError> {
        self.check(None)?;
        Ok(PageResult::empty())
    }

    async fn read_active_projection_bindings(
        &self,
    ) -> Result<HashSet<ProjectionBinding>, StoreError> {
        self.check(None)?;
        Ok(HashSet::new())
    }

    async fn read_active_quota_counts(&self) -> Result<ActiveQuotaCounts, StoreError> {
        self.check(None)?;
        Ok(ActiveQuotaCounts::default())
    }
}
