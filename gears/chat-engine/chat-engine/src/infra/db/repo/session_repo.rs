//! `sessions` repository.
//!
//! Owns CRUD for the `sessions` table:
//! - tenant + user scoped reads (`find_by_id`, `list_paginated`)
//! - lifecycle-state writes (`update_lifecycle_state`, `soft_delete`,
//!   `hard_delete`)
//! - metadata/capabilities writes
//!
//! All queries filter by `tenant_id` AND `user_id` from the caller's
//! [`crate::api::auth::Identity`] (sourced from the JWT — never from the
//! request body). Cross-tenant misses surface as `Ok(None)` so handlers can
//! map to HTTP 404 (anti-enumeration rule, per ADR-0021).
//!
//! Hard delete uses a SERIALIZABLE transaction that cascades messages and
//! reactions before removing the session row. The cascade is bounded to the
//! caller's `(tenant_id, user_id)` scope so a malicious request cannot
//! delete rows it doesn't own.
//
// @cpt-cf-chat-engine-session-repo:p4
// @cpt-cf-chat-engine-adr-session-deletion-strategy:p4
// @cpt-cf-chat-engine-adr-session-metadata:p4

use std::borrow::Cow;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait};
use serde_json::Value as JsonValue;
use time::OffsetDateTime;
use toolkit_db::odata::{LimitCfg, paginate_odata};
use toolkit_db::secure::{
    AccessScope, DBRunner, SecureDeleteExt, SecureEntityExt, SecureInsertExt, SecureUpdateExt,
    TxConfig,
};
use toolkit_odata::{ODataQuery, Page, SortDir};
use uuid::Uuid;

use crate::domain::authz::bypass;
use crate::domain::error::ChatEngineError;
use crate::domain::ports::{NewSession, SessionRepo};
use crate::domain::session::{LifecycleState, Session};
use crate::infra::db::entity::message::{self as message_entity, Entity as MessageEntity};
use crate::infra::db::entity::message_reaction::{
    self as reaction_entity, Entity as ReactionEntity,
};
use crate::infra::db::entity::session::{self as session_entity, Entity as SessionEntity};
use crate::infra::db::odata_mapper::{SessionODataMapper, SessionQueryFilterField};
use crate::infra::db::repo::{ChatEngineDb, parse_owner_uuid};

/// Parse a `sessions` owner identifier (tenant/user) into its `Uuid` column
/// value. The strings always originate from a `SecurityContext` UUID, so a
/// parse failure is an internal invariant break rather than a client error —
/// surfaced as a fail-closed internal error instead of a panic.
/// Maximum results returned per page.
pub const MAX_PAGE_SIZE: u32 = 100;

/// Default page size used when the caller does not supply `?limit=`.
pub const DEFAULT_PAGE_SIZE: u32 = 20;

/// Sea-ORM-backed implementation of [`SessionRepo`].
///
/// Holds the toolkit-db `DBProvider` so every method runs against the same
/// connection the migration runner used. `sessions` carries `String`
/// tenant/user identifiers rather than `Uuid`, so the entity is marked
/// `#[secure(unrestricted)]` and scoping stays explicit in the per-method
/// `.filter(Column::TenantId.eq(...))` clauses. The `.secure()` /
/// `.scope_with(...)` wrappers exist purely to give us a `&impl DBRunner`
/// execution path; a follow-up that lifts the columns to `Uuid` can
/// replace the manual filters with proper `AccessScope` constraints.
pub struct SeaSessionRepo {
    db: Arc<ChatEngineDb>,
}

impl SeaSessionRepo {
    #[must_use]
    pub fn new(db: Arc<ChatEngineDb>) -> Self {
        Self { db }
    }

    /// Build the `(session_id, tenant_id, user_id)` AND filter every scoped
    /// write reuses. Centralising it avoids drift between read and update
    /// paths.
    fn owned_filter(
        session_id: Uuid,
        tenant_id: &str,
        user_id: &str,
    ) -> Result<Condition, ChatEngineError> {
        // `sessions.tenant_id` / `user_id` are UUID columns; parse the caller's
        // ids once and compare Uuid-to-Uuid (a String predicate is a type error
        // on Postgres, and only silently works on typeless SQLite).
        let tenant_uuid = parse_owner_uuid(tenant_id, "tenant_id")?;
        let user_uuid = parse_owner_uuid(user_id, "user_id")?;
        Ok(Condition::all()
            .add(session_entity::Column::SessionId.eq(session_id))
            .add(session_entity::Column::TenantId.eq(tenant_uuid))
            .add(session_entity::Column::UserId.eq(user_uuid)))
    }

    /// Re-read a session by `(tenant_id, user_id, session_id)` after a
    /// scoped write. Returns `NotFound` when the row vanished — useful as a
    /// safety net for the write paths.
    async fn read_owned<R: DBRunner>(
        runner: &R,
        tenant_id: &str,
        user_id: &str,
        session_id: Uuid,
    ) -> Result<Session, ChatEngineError> {
        // AUTHZ-BYPASS: legacy owner-filtered / unrestricted path; row scoping via explicit WHERE
        // @cpt-cf-chat-engine-design-authz-bypass-registry
        let scope = bypass::unrestricted_table_scope();
        let row = SessionEntity::find()
            .secure()
            .scope_with(&scope)
            .filter(Self::owned_filter(session_id, tenant_id, user_id)?)
            .one(runner)
            .await?;
        let row = row.ok_or_else(|| ChatEngineError::not_found("session", session_id))?;
        if row.lifecycle_state == LifecycleState::HardDeleted.as_str() {
            return Err(ChatEngineError::not_found("session", session_id));
        }
        Ok(row.into())
    }

    /// Re-read a session by `session_id` under a PDP-derived scope after a
    /// scoped write. Owner/tenant filtering is carried by `scope` (applied by
    /// SecureORM); a row that falls outside the scope, is absent, or is
    /// `HardDeleted` surfaces as `NotFound` (TOCTOU-safe: the owner pair is
    /// immutable). Phase 4 counterpart of [`Self::read_owned`].
    async fn read_scoped<R: DBRunner>(
        runner: &R,
        scope: &AccessScope,
        session_id: Uuid,
    ) -> Result<Session, ChatEngineError> {
        let row = SessionEntity::find()
            .secure()
            .scope_with(scope)
            .filter(Condition::all().add(session_entity::Column::SessionId.eq(session_id)))
            .one(runner)
            .await?;
        let row = row.ok_or_else(|| ChatEngineError::not_found("session", session_id))?;
        if row.lifecycle_state == LifecycleState::HardDeleted.as_str() {
            return Err(ChatEngineError::not_found("session", session_id));
        }
        Ok(row.into())
    }
}

#[async_trait]
impl SessionRepo for SeaSessionRepo {
    async fn insert(&self, new: NewSession) -> Result<Session, ChatEngineError> {
        use sea_orm::ActiveValue::{NotSet, Set};
        let tenant_uuid = parse_owner_uuid(&new.tenant_id, "tenant_id")?;
        let user_uuid = parse_owner_uuid(&new.user_id, "user_id")?;
        let model = session_entity::ActiveModel {
            session_id: Set(new.session_id),
            tenant_id: Set(tenant_uuid),
            user_id: Set(user_uuid),
            client_id: Set(new.client_id),
            session_type_id: Set(new.session_type_id),
            enabled_capabilities: Set(None),
            metadata: Set(new.metadata),
            lifecycle_state: Set(LifecycleState::Active.as_str().to_string()),
            share_token: Set(None),
            deleted_at: NotSet,
            scheduled_hard_delete_at: NotSet,
            created_at: Set(new.created_at),
            updated_at: Set(new.updated_at),
        };
        let conn = self.db.conn()?;
        // AUTHZ-BYPASS: legacy owner-filtered / unrestricted path; row scoping via explicit WHERE
        // @cpt-cf-chat-engine-design-authz-bypass-registry
        let scope = bypass::unrestricted_table_scope();
        let inserted = SessionEntity::insert(model)
            .secure()
            .scope_unchecked(&scope)?
            .exec_with_returning(&conn)
            .await?;
        Ok(inserted.into())
    }

    async fn scheduled_hard_delete_at(
        &self,
        tenant_id: &str,
        user_id: &str,
        session_id: Uuid,
    ) -> Result<Option<OffsetDateTime>, ChatEngineError> {
        let conn = self.db.conn()?;
        // AUTHZ-BYPASS: legacy owner-filtered / unrestricted path; row scoping via explicit WHERE
        // @cpt-cf-chat-engine-design-authz-bypass-registry
        let scope = bypass::unrestricted_table_scope();
        let row = SessionEntity::find()
            .secure()
            .scope_with(&scope)
            .filter(Self::owned_filter(session_id, tenant_id, user_id)?)
            .one(&conn)
            .await?;
        Ok(row.and_then(|m| m.scheduled_hard_delete_at))
    }

    async fn find_by_id(
        &self,
        tenant_id: &str,
        user_id: &str,
        session_id: Uuid,
    ) -> Result<Option<Session>, ChatEngineError> {
        let conn = self.db.conn()?;
        // AUTHZ-BYPASS: legacy owner-filtered / unrestricted path; row scoping via explicit WHERE
        // @cpt-cf-chat-engine-design-authz-bypass-registry
        let scope = bypass::unrestricted_table_scope();
        let row = SessionEntity::find()
            .secure()
            .scope_with(&scope)
            .filter(Self::owned_filter(session_id, tenant_id, user_id)?)
            .one(&conn)
            .await?;

        Ok(row
            .filter(|m| m.lifecycle_state != LifecycleState::HardDeleted.as_str())
            .map(Into::into))
    }

    async fn list_paginated(
        &self,
        tenant_id: &str,
        user_id: &str,
        query: &ODataQuery,
    ) -> Result<Page<Session>, ChatEngineError> {
        let conn = self.db.conn()?;
        // AUTHZ-BYPASS: legacy owner-filtered / unrestricted path; row scoping via explicit WHERE
        // @cpt-cf-chat-engine-design-authz-bypass-registry
        let scope = bypass::unrestricted_table_scope();

        // UUID columns: parse the caller's ids once and compare Uuid-to-Uuid.
        let tenant_uuid = parse_owner_uuid(tenant_id, "tenant_id")?;
        let user_uuid = parse_owner_uuid(user_id, "user_id")?;

        // Tenant / user scoping and the hard-delete exclusion come from the
        // caller's identity, never from the OData `$filter`, so they live in
        // the base query rather than the caller-controlled clause.
        let base_query = SessionEntity::find().secure().scope_with(&scope).filter(
            Condition::all()
                .add(session_entity::Column::TenantId.eq(tenant_uuid))
                .add(session_entity::Column::UserId.eq(user_uuid))
                .add(
                    session_entity::Column::LifecycleState
                        .ne(LifecycleState::HardDeleted.as_str().to_string()),
                ),
        );

        // Default-recent posture: when the caller supplies neither a cursor
        // nor `$orderby`, sort by `created_at DESC`. `session_id` is appended
        // as the unique tiebreaker by `paginate_odata`, yielding the legacy
        // `(created_at DESC, session_id DESC)` total order.
        let query = if query.cursor.is_none() && query.order.is_empty() {
            let mut adjusted = query.clone();
            adjusted.order = adjusted
                .order
                .ensure_tiebreaker("created_at", SortDir::Desc);
            Cow::Owned(adjusted)
        } else {
            Cow::Borrowed(query)
        };

        let limit_cfg = LimitCfg {
            default: u64::from(DEFAULT_PAGE_SIZE),
            max: u64::from(MAX_PAGE_SIZE),
        };

        paginate_odata::<SessionQueryFilterField, SessionODataMapper, _, _, _, _>(
            base_query,
            &conn,
            query.as_ref(),
            ("session_id", SortDir::Desc),
            limit_cfg,
            |m: session_entity::Model| Session::from(m),
        )
        .await
        .map_err(map_odata_err)
    }

    async fn update_metadata(
        &self,
        tenant_id: &str,
        user_id: &str,
        session_id: Uuid,
        metadata: Option<JsonValue>,
    ) -> Result<Session, ChatEngineError> {
        // Read-then-update inside one transaction so the read enforces
        // ownership before the write touches the row, and so concurrent
        // writers can't slip a different tenant in between the two.
        let tenant_id = tenant_id.to_owned();
        let user_id = user_id.to_owned();
        self.db
            .transaction(move |tx| {
                Box::pin(async move {
                    let _existing = Self::read_owned(tx, &tenant_id, &user_id, session_id).await?;
                    let now = OffsetDateTime::now_utc();
                    // AUTHZ-BYPASS: legacy owner-filtered / unrestricted path; row scoping via explicit WHERE
                    // @cpt-cf-chat-engine-design-authz-bypass-registry
                    let scope = bypass::unrestricted_table_scope();
                    SessionEntity::update_many()
                        .secure()
                        .scope_with(&scope)
                        .filter(Self::owned_filter(session_id, &tenant_id, &user_id)?)
                        .col_expr(
                            session_entity::Column::Metadata,
                            Expr::value(metadata.clone()),
                        )
                        .col_expr(session_entity::Column::UpdatedAt, Expr::value(now))
                        .exec(tx)
                        .await?;
                    Self::read_owned(tx, &tenant_id, &user_id, session_id).await
                })
            })
            .await
    }

    async fn update_capabilities(
        &self,
        tenant_id: &str,
        user_id: &str,
        session_id: Uuid,
        capabilities: Option<JsonValue>,
    ) -> Result<Session, ChatEngineError> {
        let tenant_id = tenant_id.to_owned();
        let user_id = user_id.to_owned();
        self.db
            .transaction(move |tx| {
                Box::pin(async move {
                    let _existing = Self::read_owned(tx, &tenant_id, &user_id, session_id).await?;
                    let now = OffsetDateTime::now_utc();
                    // AUTHZ-BYPASS: legacy owner-filtered / unrestricted path; row scoping via explicit WHERE
                    // @cpt-cf-chat-engine-design-authz-bypass-registry
                    let scope = bypass::unrestricted_table_scope();
                    SessionEntity::update_many()
                        .secure()
                        .scope_with(&scope)
                        .filter(Self::owned_filter(session_id, &tenant_id, &user_id)?)
                        .col_expr(
                            session_entity::Column::EnabledCapabilities,
                            Expr::value(capabilities.clone()),
                        )
                        .col_expr(session_entity::Column::UpdatedAt, Expr::value(now))
                        .exec(tx)
                        .await?;
                    Self::read_owned(tx, &tenant_id, &user_id, session_id).await
                })
            })
            .await
    }

    async fn update_lifecycle_state(
        &self,
        tenant_id: &str,
        user_id: &str,
        session_id: Uuid,
        new_state: LifecycleState,
    ) -> Result<Session, ChatEngineError> {
        let tenant_id = tenant_id.to_owned();
        let user_id = user_id.to_owned();
        let state_str = new_state.as_str().to_string();
        let clear_deletion = matches!(new_state, LifecycleState::Active);
        self.db
            .transaction(move |tx| {
                Box::pin(async move {
                    let _existing = Self::read_owned(tx, &tenant_id, &user_id, session_id).await?;
                    let now = OffsetDateTime::now_utc();
                    // AUTHZ-BYPASS: legacy owner-filtered / unrestricted path; row scoping via explicit WHERE
                    // @cpt-cf-chat-engine-design-authz-bypass-registry
                    let scope = bypass::unrestricted_table_scope();
                    let mut update = SessionEntity::update_many()
                        .secure()
                        .scope_with(&scope)
                        .filter(Self::owned_filter(session_id, &tenant_id, &user_id)?)
                        .col_expr(
                            session_entity::Column::LifecycleState,
                            Expr::value(state_str.clone()),
                        )
                        .col_expr(session_entity::Column::UpdatedAt, Expr::value(now));
                    if clear_deletion {
                        // Restoring out of soft_deleted clears the deletion
                        // bookkeeping; archive transitions leave it intact (a
                        // soft-deleted session is the only state that uses
                        // those columns).
                        let null_dt: Option<OffsetDateTime> = None;
                        update = update
                            .col_expr(session_entity::Column::DeletedAt, Expr::value(null_dt))
                            .col_expr(
                                session_entity::Column::ScheduledHardDeleteAt,
                                Expr::value(null_dt),
                            );
                    }
                    update.exec(tx).await?;
                    Self::read_owned(tx, &tenant_id, &user_id, session_id).await
                })
            })
            .await
    }

    async fn soft_delete(
        &self,
        tenant_id: &str,
        user_id: &str,
        session_id: Uuid,
        retention_days: i64,
    ) -> Result<Session, ChatEngineError> {
        let tenant_id = tenant_id.to_owned();
        let user_id = user_id.to_owned();
        self.db
            .transaction(move |tx| {
                Box::pin(async move {
                    let _existing = Self::read_owned(tx, &tenant_id, &user_id, session_id).await?;
                    let now = OffsetDateTime::now_utc();
                    let grace = retention_days.max(0);
                    let scheduled = now + Duration::from_secs((grace as u64) * 86_400);
                    // AUTHZ-BYPASS: legacy owner-filtered / unrestricted path; row scoping via explicit WHERE
                    // @cpt-cf-chat-engine-design-authz-bypass-registry
                    let scope = bypass::unrestricted_table_scope();
                    SessionEntity::update_many()
                        .secure()
                        .scope_with(&scope)
                        .filter(Self::owned_filter(session_id, &tenant_id, &user_id)?)
                        .col_expr(
                            session_entity::Column::LifecycleState,
                            Expr::value(LifecycleState::SoftDeleted.as_str().to_string()),
                        )
                        .col_expr(session_entity::Column::DeletedAt, Expr::value(Some(now)))
                        .col_expr(
                            session_entity::Column::ScheduledHardDeleteAt,
                            Expr::value(Some(scheduled)),
                        )
                        .col_expr(session_entity::Column::UpdatedAt, Expr::value(now))
                        .exec(tx)
                        .await?;
                    Self::read_owned(tx, &tenant_id, &user_id, session_id).await
                })
            })
            .await
    }

    async fn hard_delete(
        &self,
        tenant_id: &str,
        user_id: &str,
        session_id: Uuid,
    ) -> Result<bool, ChatEngineError> {
        // SERIALIZABLE isolation: per ADR-0021 the cascade MUST observe a
        // consistent snapshot so concurrent message inserts cannot race the
        // deletion. Conflicts surface as serialization failures and bubble
        // out to the caller as 409 (Phase 14 will map them).
        let tenant_id = tenant_id.to_owned();
        let user_id = user_id.to_owned();
        self.db
            .transaction_with_config(TxConfig::serializable(), move |tx| {
                Box::pin(async move {
                    // AUTHZ-BYPASS: legacy owner-filtered / unrestricted path; row scoping via explicit WHERE
                    // @cpt-cf-chat-engine-design-authz-bypass-registry
                    let scope = bypass::unrestricted_table_scope();
                    // Re-load inside the transaction to make sure the
                    // ownership/lifecycle check sees the same snapshot as
                    // the cascade deletes.
                    let owned = SessionEntity::find()
                        .secure()
                        .scope_with(&scope)
                        .filter(SeaSessionRepo::owned_filter(
                            session_id, &tenant_id, &user_id,
                        )?)
                        .one(tx)
                        .await?;

                    let Some(_session) = owned else {
                        return Ok(false);
                    };

                    // Load (id, parent) for every message so we can delete the
                    // tree leaves-first below.
                    let messages: Vec<(Uuid, Option<Uuid>)> = MessageEntity::find()
                        .secure()
                        .scope_with(&scope)
                        .filter(
                            Condition::all().add(message_entity::Column::SessionId.eq(session_id)),
                        )
                        .all(tx)
                        .await?
                        .into_iter()
                        .map(|m| (m.message_id, m.parent_message_id))
                        .collect();

                    if !messages.is_empty() {
                        let message_ids: Vec<Uuid> = messages.iter().map(|(id, _)| *id).collect();
                        // Cascade reactions first (they FK onto messages).
                        ReactionEntity::delete_many()
                            .secure()
                            .scope_with(&scope)
                            .filter(
                                Condition::all().add(
                                    reaction_entity::Column::MessageId.is_in(message_ids.clone()),
                                ),
                            )
                            .exec(tx)
                            .await?;

                        // Delete messages leaves-first: `fk_messages_parent` is
                        // RESTRICT, so a single bulk delete trips the
                        // self-referential FK when a parent row is removed while
                        // a child still references it. We can't NULL the parent
                        // links instead — that collapses children to roots and
                        // collides on the `uq_messages_session_root_variant`
                        // partial unique index. Each wave deletes the current
                        // leaves (ids that are not a parent of any survivor);
                        // `message_parts` cascade off `fk_message_parts_message`.
                        let mut remaining: std::collections::HashMap<Uuid, Option<Uuid>> =
                            messages.into_iter().collect();
                        while !remaining.is_empty() {
                            let parents: std::collections::HashSet<Uuid> = remaining
                                .values()
                                .filter_map(|p| *p)
                                .filter(|p| remaining.contains_key(p))
                                .collect();
                            let leaves: Vec<Uuid> = remaining
                                .keys()
                                .copied()
                                .filter(|id| !parents.contains(id))
                                .collect();
                            MessageEntity::delete_many()
                                .secure()
                                .scope_with(&scope)
                                .filter(
                                    Condition::all().add(
                                        message_entity::Column::MessageId.is_in(leaves.clone()),
                                    ),
                                )
                                .exec(tx)
                                .await?;
                            for id in &leaves {
                                remaining.remove(id);
                            }
                        }
                    }

                    SessionEntity::delete_many()
                        .secure()
                        .scope_with(&scope)
                        .filter(SeaSessionRepo::owned_filter(
                            session_id, &tenant_id, &user_id,
                        )?)
                        .exec(tx)
                        .await?;

                    Ok(true)
                })
            })
            .await
    }

    async fn list_active_sessions_for_tenant(
        &self,
        tenant_id: &str,
        after: Option<Uuid>,
        limit: u32,
    ) -> Result<Vec<Session>, ChatEngineError> {
        let conn = self.db.conn()?;
        // AUTHZ-BYPASS: system op; not HTTP-exposed
        // @cpt-cf-chat-engine-design-authz-bypass-registry
        let scope = bypass::system_read_scope();
        // UUID column: parse the caller's tenant id once and compare Uuid-to-Uuid.
        let tenant_uuid = parse_owner_uuid(tenant_id, "tenant_id")?;
        let mut filter = Condition::all()
            .add(session_entity::Column::TenantId.eq(tenant_uuid))
            .add(
                session_entity::Column::LifecycleState
                    .eq(LifecycleState::Active.as_str().to_string()),
            );
        if let Some(after) = after {
            // Keyset cursor on the `session_id` primary key — pages the
            // retention sweep across ticks without OFFSET scans.
            filter = filter.add(session_entity::Column::SessionId.gt(after));
        }
        // Push the per-tick cap into SQL so a large tenant returns only the
        // candidates this tick will process, not its entire active set.
        let rows = SessionEntity::find()
            .secure()
            .scope_with(&scope)
            .filter(filter)
            .order_by(session_entity::Column::SessionId, sea_orm::Order::Asc)
            .limit(u64::from(limit))
            .all(&conn)
            .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    async fn list_tenants_with_active_sessions(&self) -> Result<Vec<String>, ChatEngineError> {
        // The secure layer doesn't yet expose `select_only() +
        // into_tuple()`, so the original `SELECT DISTINCT tenant_id`
        // projection has no `&impl DBRunner` execution path. Fall back to
        // selecting the active rows and deduplicating in-process — the
        // retention scheduler is the only caller, runs on an interval, and
        // is bounded by the active-session count which the cleanup task
        // already polls regularly. Once `SecureSelect::into_tuple` exists
        // this collapses back to a one-shot `DISTINCT` scan.
        let conn = self.db.conn()?;
        // AUTHZ-BYPASS: system op; not HTTP-exposed
        // @cpt-cf-chat-engine-design-authz-bypass-registry
        let scope = bypass::system_read_scope();
        let rows = SessionEntity::find()
            .secure()
            .scope_with(&scope)
            .filter(
                Condition::all().add(
                    session_entity::Column::LifecycleState
                        .eq(LifecycleState::Active.as_str().to_string()),
                ),
            )
            .all(&conn)
            .await?;

        let mut tenants: Vec<String> = rows.into_iter().map(|m| m.tenant_id.to_string()).collect();
        tenants.sort_unstable();
        tenants.dedup();
        Ok(tenants)
    }

    async fn find_by_session_id_unscoped(
        &self,
        session_id: Uuid,
    ) -> Result<Option<Session>, ChatEngineError> {
        let conn = self.db.conn()?;
        // AUTHZ-BYPASS: system op; not HTTP-exposed
        // @cpt-cf-chat-engine-design-authz-bypass-registry
        let scope = bypass::system_read_scope();
        let row = SessionEntity::find()
            .secure()
            .scope_with(&scope)
            .filter(Condition::all().add(session_entity::Column::SessionId.eq(session_id)))
            .one(&conn)
            .await?;
        Ok(row
            .filter(|m| m.lifecycle_state != LifecycleState::HardDeleted.as_str())
            .map(Into::into))
    }

    async fn find_by_share_token(
        &self,
        share_token: &str,
    ) -> Result<Option<Session>, ChatEngineError> {
        // UNIQUE index on `sessions.share_token` is exercised by this
        // single equality lookup — keep the query verbatim so EXPLAIN
        // selects the index per ADR-0016.
        let conn = self.db.conn()?;
        // AUTHZ-BYPASS: capability-URL read; token is the grant; 404 on miss/revoked
        // @cpt-cf-chat-engine-seq-authz-shared-read
        let scope = bypass::capability_read_scope();
        let row = SessionEntity::find()
            .secure()
            .scope_with(&scope)
            .filter(
                Condition::all().add(session_entity::Column::ShareToken.eq(share_token.to_owned())),
            )
            .one(&conn)
            .await?;
        Ok(row
            .filter(|m| m.lifecycle_state != LifecycleState::HardDeleted.as_str())
            .map(Into::into))
    }

    async fn update_share_token(
        &self,
        tenant_id: &str,
        user_id: &str,
        session_id: Uuid,
        share_token: Option<String>,
        metadata: Option<JsonValue>,
    ) -> Result<Session, ChatEngineError> {
        let tenant_id = tenant_id.to_owned();
        let user_id = user_id.to_owned();
        self.db
            .transaction(move |tx| {
                Box::pin(async move {
                    let _existing = Self::read_owned(tx, &tenant_id, &user_id, session_id).await?;
                    let now = OffsetDateTime::now_utc();
                    // AUTHZ-BYPASS: legacy owner-filtered / unrestricted path; row scoping via explicit WHERE
                    // @cpt-cf-chat-engine-design-authz-bypass-registry
                    let scope = bypass::unrestricted_table_scope();
                    SessionEntity::update_many()
                        .secure()
                        .scope_with(&scope)
                        .filter(Self::owned_filter(session_id, &tenant_id, &user_id)?)
                        .col_expr(
                            session_entity::Column::ShareToken,
                            Expr::value(share_token.clone()),
                        )
                        .col_expr(
                            session_entity::Column::Metadata,
                            Expr::value(metadata.clone()),
                        )
                        .col_expr(session_entity::Column::UpdatedAt, Expr::value(now))
                        .exec(tx)
                        .await?;
                    Self::read_owned(tx, &tenant_id, &user_id, session_id).await
                })
            })
            .await
    }

    // ---------------------------------------------------------------------
    // Phase 4 (PEP) — PDP-scoped surface. Owner/tenant filtering is carried by
    // the PDP-derived `AccessScope` (applied via SecureORM `.scope_with`),
    // so the residual predicate is just `session_id`.
    // @cpt-cf-chat-engine-interface-pep
    // ---------------------------------------------------------------------

    async fn insert_scoped(
        &self,
        scope: &AccessScope,
        new: NewSession,
    ) -> Result<Session, ChatEngineError> {
        use sea_orm::ActiveValue::{NotSet, Set};
        let tenant_uuid = parse_owner_uuid(&new.tenant_id, "tenant_id")?;
        let user_uuid = parse_owner_uuid(&new.user_id, "user_id")?;
        let model = session_entity::ActiveModel {
            session_id: Set(new.session_id),
            tenant_id: Set(tenant_uuid),
            user_id: Set(user_uuid),
            client_id: Set(new.client_id),
            session_type_id: Set(new.session_type_id),
            enabled_capabilities: Set(None),
            metadata: Set(new.metadata),
            lifecycle_state: Set(LifecycleState::Active.as_str().to_string()),
            share_token: Set(None),
            deleted_at: NotSet,
            scheduled_hard_delete_at: NotSet,
            created_at: Set(new.created_at),
            updated_at: Set(new.updated_at),
        };
        let conn = self.db.conn()?;
        // Validate the row against the PDP-derived CREATE scope: a constrained
        // decision (owner/resource predicates) must be satisfied by the
        // persisted tenant_id / user_id, else the insert is `Denied`. The
        // unchecked path is reserved for documented internal bypasses, not a
        // request-scoped create.
        let inserted = SessionEntity::insert(model.clone())
            .secure()
            .scope_with_model(scope, &model)?
            .exec_with_returning(&conn)
            .await?;
        Ok(inserted.into())
    }

    async fn find_by_id_scoped(
        &self,
        scope: &AccessScope,
        session_id: Uuid,
    ) -> Result<Option<Session>, ChatEngineError> {
        let conn = self.db.conn()?;
        let row = SessionEntity::find()
            .secure()
            .scope_with(scope)
            .filter(Condition::all().add(session_entity::Column::SessionId.eq(session_id)))
            .one(&conn)
            .await?;
        Ok(row
            .filter(|m| m.lifecycle_state != LifecycleState::HardDeleted.as_str())
            .map(Into::into))
    }

    async fn list_scoped(
        &self,
        scope: &AccessScope,
        query: &ODataQuery,
    ) -> Result<Page<Session>, ChatEngineError> {
        let conn = self.db.conn()?;

        // Owner/tenant scoping now comes from the PDP-derived scope (applied by
        // SecureORM). Only the hard-delete exclusion — an engine invariant, not
        // a caller-controlled `$filter` — lives in the base query.
        let base_query = SessionEntity::find().secure().scope_with(scope).filter(
            Condition::all().add(
                session_entity::Column::LifecycleState
                    .ne(LifecycleState::HardDeleted.as_str().to_string()),
            ),
        );

        let query = if query.cursor.is_none() && query.order.is_empty() {
            let mut adjusted = query.clone();
            adjusted.order = adjusted
                .order
                .ensure_tiebreaker("created_at", SortDir::Desc);
            Cow::Owned(adjusted)
        } else {
            Cow::Borrowed(query)
        };

        let limit_cfg = LimitCfg {
            default: u64::from(DEFAULT_PAGE_SIZE),
            max: u64::from(MAX_PAGE_SIZE),
        };

        paginate_odata::<SessionQueryFilterField, SessionODataMapper, _, _, _, _>(
            base_query,
            &conn,
            query.as_ref(),
            ("session_id", SortDir::Desc),
            limit_cfg,
            |m: session_entity::Model| Session::from(m),
        )
        .await
        .map_err(map_odata_err)
    }

    async fn update_metadata_scoped(
        &self,
        scope: &AccessScope,
        session_id: Uuid,
        metadata: Option<JsonValue>,
    ) -> Result<Session, ChatEngineError> {
        let scope = scope.clone();
        self.db
            .transaction(move |tx| {
                Box::pin(async move {
                    let _existing = Self::read_scoped(tx, &scope, session_id).await?;
                    let now = OffsetDateTime::now_utc();
                    SessionEntity::update_many()
                        .secure()
                        .scope_with(&scope)
                        .filter(
                            Condition::all().add(session_entity::Column::SessionId.eq(session_id)),
                        )
                        .col_expr(
                            session_entity::Column::Metadata,
                            Expr::value(metadata.clone()),
                        )
                        .col_expr(session_entity::Column::UpdatedAt, Expr::value(now))
                        .exec(tx)
                        .await?;
                    Self::read_scoped(tx, &scope, session_id).await
                })
            })
            .await
    }

    async fn update_capabilities_scoped(
        &self,
        scope: &AccessScope,
        session_id: Uuid,
        capabilities: Option<JsonValue>,
    ) -> Result<Session, ChatEngineError> {
        let scope = scope.clone();
        self.db
            .transaction(move |tx| {
                Box::pin(async move {
                    let _existing = Self::read_scoped(tx, &scope, session_id).await?;
                    let now = OffsetDateTime::now_utc();
                    SessionEntity::update_many()
                        .secure()
                        .scope_with(&scope)
                        .filter(
                            Condition::all().add(session_entity::Column::SessionId.eq(session_id)),
                        )
                        .col_expr(
                            session_entity::Column::EnabledCapabilities,
                            Expr::value(capabilities.clone()),
                        )
                        .col_expr(session_entity::Column::UpdatedAt, Expr::value(now))
                        .exec(tx)
                        .await?;
                    Self::read_scoped(tx, &scope, session_id).await
                })
            })
            .await
    }

    async fn update_metadata_and_capabilities_scoped(
        &self,
        scope: &AccessScope,
        session_id: Uuid,
        metadata: Option<JsonValue>,
        capabilities: Option<JsonValue>,
    ) -> Result<Session, ChatEngineError> {
        let scope = scope.clone();
        self.db
            .transaction(move |tx| {
                Box::pin(async move {
                    let _existing = Self::read_scoped(tx, &scope, session_id).await?;
                    let now = OffsetDateTime::now_utc();
                    // Single UPDATE sets both columns atomically — either both
                    // land or neither does.
                    SessionEntity::update_many()
                        .secure()
                        .scope_with(&scope)
                        .filter(
                            Condition::all().add(session_entity::Column::SessionId.eq(session_id)),
                        )
                        .col_expr(
                            session_entity::Column::Metadata,
                            Expr::value(metadata.clone()),
                        )
                        .col_expr(
                            session_entity::Column::EnabledCapabilities,
                            Expr::value(capabilities.clone()),
                        )
                        .col_expr(session_entity::Column::UpdatedAt, Expr::value(now))
                        .exec(tx)
                        .await?;
                    Self::read_scoped(tx, &scope, session_id).await
                })
            })
            .await
    }

    async fn update_lifecycle_state_scoped(
        &self,
        scope: &AccessScope,
        session_id: Uuid,
        new_state: LifecycleState,
    ) -> Result<Session, ChatEngineError> {
        let scope = scope.clone();
        let state_str = new_state.as_str().to_string();
        let clear_deletion = matches!(new_state, LifecycleState::Active);
        self.db
            .transaction(move |tx| {
                Box::pin(async move {
                    let _existing = Self::read_scoped(tx, &scope, session_id).await?;
                    let now = OffsetDateTime::now_utc();
                    let mut update = SessionEntity::update_many()
                        .secure()
                        .scope_with(&scope)
                        .filter(
                            Condition::all().add(session_entity::Column::SessionId.eq(session_id)),
                        )
                        .col_expr(
                            session_entity::Column::LifecycleState,
                            Expr::value(state_str.clone()),
                        )
                        .col_expr(session_entity::Column::UpdatedAt, Expr::value(now));
                    if clear_deletion {
                        let null_dt: Option<OffsetDateTime> = None;
                        update = update
                            .col_expr(session_entity::Column::DeletedAt, Expr::value(null_dt))
                            .col_expr(
                                session_entity::Column::ScheduledHardDeleteAt,
                                Expr::value(null_dt),
                            );
                    }
                    update.exec(tx).await?;
                    Self::read_scoped(tx, &scope, session_id).await
                })
            })
            .await
    }

    async fn soft_delete_scoped(
        &self,
        scope: &AccessScope,
        session_id: Uuid,
        retention_days: i64,
    ) -> Result<Session, ChatEngineError> {
        let scope = scope.clone();
        self.db
            .transaction(move |tx| {
                Box::pin(async move {
                    let _existing = Self::read_scoped(tx, &scope, session_id).await?;
                    let now = OffsetDateTime::now_utc();
                    let grace = retention_days.max(0);
                    let scheduled = now + Duration::from_secs((grace as u64) * 86_400);
                    SessionEntity::update_many()
                        .secure()
                        .scope_with(&scope)
                        .filter(
                            Condition::all().add(session_entity::Column::SessionId.eq(session_id)),
                        )
                        .col_expr(
                            session_entity::Column::LifecycleState,
                            Expr::value(LifecycleState::SoftDeleted.as_str().to_string()),
                        )
                        .col_expr(session_entity::Column::DeletedAt, Expr::value(Some(now)))
                        .col_expr(
                            session_entity::Column::ScheduledHardDeleteAt,
                            Expr::value(Some(scheduled)),
                        )
                        .col_expr(session_entity::Column::UpdatedAt, Expr::value(now))
                        .exec(tx)
                        .await?;
                    Self::read_scoped(tx, &scope, session_id).await
                })
            })
            .await
    }

    async fn scheduled_hard_delete_at_scoped(
        &self,
        scope: &AccessScope,
        session_id: Uuid,
    ) -> Result<Option<OffsetDateTime>, ChatEngineError> {
        let conn = self.db.conn()?;
        let row = SessionEntity::find()
            .secure()
            .scope_with(scope)
            .filter(Condition::all().add(session_entity::Column::SessionId.eq(session_id)))
            .one(&conn)
            .await?;
        Ok(row.and_then(|m| m.scheduled_hard_delete_at))
    }
}

/// Classify an `OData` pagination failure into a [`ChatEngineError`].
///
/// Filter / orderby / cursor problems are caller mistakes (HTTP 400); a
/// `Db` failure is an internal fault (HTTP 500).
fn map_odata_err(err: toolkit_odata::Error) -> ChatEngineError {
    match err {
        toolkit_odata::Error::Db(msg) => ChatEngineError::internal(msg),
        other => ChatEngineError::bad_request(other.to_string()),
    }
}

#[cfg(test)]
#[path = "session_repo_tests.rs"]
mod session_repo_tests;
