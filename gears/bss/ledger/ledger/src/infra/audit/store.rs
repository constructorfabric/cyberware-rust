//! `SecuredAuditStore` — the append-only secured-audit writer. `append`
//! generates a v7 audit id, reads the tenant's audit-chain tip (scoped, no
//! lock; genesis seed if absent), computes the record's `row_hash` over the
//! canonical encoding linked to the tip's `prev_hash`, INSERTs the record born
//! sealed (`row_hash` / `prev_hash` `non-NULL` — the row is never updated later),
//! and advances the `audit_chain_state` tip (`INSERT … ON CONFLICT DO UPDATE`).
//!
//! Stateless — every method runs inside the caller's transaction (`txn`), so it
//! holds no `DBProvider` (mirrors
//! [`crate::infra::posting::idempotency::IdempotencyGate`] /
//! [`crate::infra::storage::repo::ChainStateRepo`]). Tenant isolation runs
//! through the `SecureORM` layer: reads use `.secure().scope_with(scope)`;
//! the record + tip inserts use `.secure().scope_with_model(scope, &am)`, which
//! validates the `ActiveModel`'s `tenant_id` against the scope so a mismatched
//! `(scope, tenant)` pair is rejected rather than silently written cross-tenant.
//!
//! ## Concurrency
//!
//! The tip read takes no row lock (the `SecureORM` builder exposes none; see
//! [`crate::infra::storage::repo::ChainStateRepo`] module docs). Two concurrent
//! appends onto the same tenant tip overlap on the `audit_chain_state` row they
//! both read-then-write, so a `SERIALIZABLE` transaction makes Postgres SSI
//! abort the loser (which retries from a fresh tip); the `tenant_id` primary key
//! makes the `ON CONFLICT` upsert itself atomic regardless.

use sea_orm::sea_query::OnConflict;
use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, EntityTrait};
use toolkit_db::DbError;
use toolkit_db::secure::{AccessScope, DbTx, ScopeError, SecureEntityExt, SecureInsertExt};
use uuid::Uuid;

use crate::domain::audit_chain::{AuditHashInput, audit_genesis_prev_hash, audit_row_hash};
use crate::infra::audit::event_type::AuditEventType;
use crate::infra::posting::service::infra;
use crate::infra::storage::entity::{audit_chain_state, secured_audit_record};
use time::OffsetDateTime;

/// Map a [`ScopeError`] to [`DbError`] **preserving the inner `sea_orm::DbErr`
/// variant** (mirrors `period_close::scope_to_db`). This is load-bearing for
/// retry: a statement-time serialization failure (SSI 40001) surfaces as
/// `ScopeError::Db(DbErr::Exec | DbErr::Query)`; keeping that variant lets the
/// `transaction_with_retry` contention classifier recognise it and retry the
/// append from a fresh tip. Stringifying it (the old `infra(format!(…))` /
/// `RepoError::Db(format!(…))`) buried it in a `DbErr::Custom`, which the
/// classifier treats as the NON-retryable business sentinel — so two concurrent
/// appends on one tenant's audit-chain tip surfaced a statement-time abort as a
/// 500 instead of retrying.
fn scope_to_db(e: ScopeError) -> DbError {
    match e {
        ScopeError::Db(db_err) => DbError::Sea(db_err),
        other => DbError::Other(anyhow::anyhow!("audit-store scope: {other}")),
    }
}

/// The append-only secured-audit store. Stateless (holds no `DBProvider`).
#[derive(Clone, Default)]
pub struct SecuredAuditStore;

impl SecuredAuditStore {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Append a sealed secured-audit record for `tenant` inside `txn`: link it
    /// onto the tenant's audit-chain tip (or genesis), INSERT it born sealed,
    /// and advance the tip. Returns the generated `audit_id`.
    ///
    /// # Errors
    /// [`DbError`] on any storage / scope failure: a tip read/advance failure, a
    /// tip `row_hash` that is not 32 bytes, or the INSERT. The `?`/`Err` rolls
    /// the caller's transaction back.
    #[allow(
        clippy::too_many_arguments,
        reason = "one sealed audit record's full field set (event/actor/reason/before_after/correlation/retention) over the caller's txn/scope"
    )]
    pub async fn append(
        &self,
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        event_type: AuditEventType,
        actor_ref: Option<&str>,
        reason_code: Option<&str>,
        before_after: &serde_json::Value,
        correlation_id: Option<Uuid>,
        retain_until: Option<OffsetDateTime>,
    ) -> Result<Uuid, DbError> {
        let audit_id = Uuid::now_v7();
        let at_utc = OffsetDateTime::now_utc();

        // 1. Read the current tip (None at genesis).
        let tip = self.read_tip(txn, scope, tenant).await?;

        // 2. The prev_hash is the tip's row_hash, or the tenant genesis seed.
        let prev_hash: [u8; 32] = match &tip {
            Some(t) => t.last_row_hash[..]
                .try_into()
                .map_err(|_| infra("audit chain tip row_hash not 32 bytes"))?,
            None => audit_genesis_prev_hash(tenant),
        };

        // 3. Compute this record's row_hash over the canonical encoding. A
        //    serialize failure fails the append rather than sealing a
        //    record that hashes like an empty object.
        let row_hash = audit_row_hash(
            &AuditHashInput {
                audit_id,
                tenant_id: tenant,
                event_type: event_type.as_str(),
                actor_ref,
                reason_code,
                correlation_id,
                at_utc,
                before_after,
            },
            &prev_hash,
        )
        .map_err(|e| infra(format!("audit row_hash canonicalize: {e}")))?;

        // 4. INSERT the record born sealed (append-only — never UPDATEd later).
        let am = secured_audit_record::ActiveModel {
            audit_id: Set(audit_id),
            tenant_id: Set(tenant),
            event_type: Set(event_type.as_str().to_owned()),
            actor_ref: Set(actor_ref.map(str::to_owned)),
            reason_code: Set(reason_code.map(str::to_owned)),
            before_after: Set(before_after.clone()),
            correlation_id: Set(correlation_id),
            row_hash: Set(row_hash.to_vec()),
            prev_hash: Set(prev_hash.to_vec()),
            at_utc: Set(at_utc),
            retain_until: Set(retain_until),
        };
        secured_audit_record::Entity::insert(am.clone())
            .secure()
            .scope_with_model(scope, &am)
            .map_err(scope_to_db)?
            .exec(txn)
            .await
            .map_err(scope_to_db)?;

        // 5. Advance the tip to this record's sealed values.
        let next_seq = tip.as_ref().map_or(1, |t| t.last_seq + 1);
        self.advance_tip(txn, scope, tenant, &row_hash, audit_id, next_seq)
            .await?;

        Ok(audit_id)
    }

    /// Read the audit-chain tip for `tenant` inside `txn` under `scope`. Returns
    /// `None` at genesis (no tip row yet). Takes no row lock.
    async fn read_tip(
        &self,
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
    ) -> Result<Option<audit_chain_state::Model>, DbError> {
        audit_chain_state::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(Condition::all().add(audit_chain_state::Column::TenantId.eq(tenant)))
            .one(txn)
            .await
            .map_err(scope_to_db)
    }

    /// Advance (upsert) the audit-chain tip for `tenant`:
    /// `INSERT … ON CONFLICT (tenant_id) DO UPDATE SET` the three tip columns.
    async fn advance_tip(
        &self,
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        row_hash: &[u8; 32],
        audit_id: Uuid,
        seq: i64,
    ) -> Result<(), DbError> {
        let am = audit_chain_state::ActiveModel {
            tenant_id: Set(tenant),
            last_row_hash: Set(row_hash.to_vec()),
            last_audit_id: Set(audit_id),
            last_seq: Set(seq),
        };
        let on_conflict = OnConflict::column(audit_chain_state::Column::TenantId)
            .update_columns([
                audit_chain_state::Column::LastRowHash,
                audit_chain_state::Column::LastAuditId,
                audit_chain_state::Column::LastSeq,
            ])
            .to_owned();

        audit_chain_state::Entity::insert(am.clone())
            .secure()
            .scope_with_model(scope, &am)
            .map_err(scope_to_db)?
            .on_conflict_raw(on_conflict)
            .exec(txn)
            .await
            .map_err(scope_to_db)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::DbErr;

    use super::{DbError, ScopeError, scope_to_db};

    /// A `ScopeError::Db` keeps its inner `DbErr` variant — the property that
    /// lets the retry classifier recognise a serialization failure
    /// (`DbErr::Exec`/`Query`) instead of a stringified `DbErr::Custom`.
    #[test]
    fn scope_to_db_preserves_the_inner_dberr_variant() {
        let mapped = scope_to_db(ScopeError::Db(DbErr::RecordNotFound("r".to_owned())));
        assert!(
            matches!(mapped, DbError::Sea(DbErr::RecordNotFound(_))),
            "ScopeError::Db must map to DbError::Sea preserving the variant, got {mapped:?}"
        );
    }

    /// A non-`Db` scope error is a non-retryable infra fault → `DbError::Other`
    /// (not the `DbErr::Custom` business sentinel, not retryable).
    #[test]
    fn scope_to_db_maps_non_db_scope_error_to_other() {
        let mapped = scope_to_db(ScopeError::Invalid("bad scope"));
        assert!(
            matches!(mapped, DbError::Other(_)),
            "a non-Db scope error must be DbError::Other, got {mapped:?}"
        );
    }
}
