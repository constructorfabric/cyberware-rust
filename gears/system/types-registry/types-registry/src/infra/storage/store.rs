//! The adapter behind the domain's persistence ports.
//!
//! [`Repos`] implements every trait in [`crate::domain::ports`] over the
//! repositories in [`super::repo`]. It holds no state and — since the repositories
//! speak the domain's row types themselves — no mapping either: every method
//! forwards the transaction verbatim, the same shape as `credstore`'s
//! `repo_impl.rs` and `account-management`'s `repo_impl/mod.rs`.
//!
//! # Why this file exists at all, given it only forwards
//!
//! The domain holds one `Arc<dyn Stores>`, and
//! [`Stores`](crate::domain::ports::Stores) is the conjunction of six traits, so
//! something has to be a single type implementing all six — the repositories are
//! five separate unit structs. The alternative, six `Arc<dyn XStore>` in the
//! service, is more wiring at every call site for no gain.
//!
//! Only repository operations used by the domain are exposed as ports.

use async_trait::async_trait;
use time::OffsetDateTime;
use toolkit_db::DbTx;
use toolkit_db::secure::{AccessScope, ScopeError};
use uuid::Uuid;

use crate::domain::admission::fingerprint::ScopeHash;
use crate::domain::enums::{DependencyKind, EntityKind, OwnershipScope};
use crate::domain::family::FamilyKey;
use crate::domain::ports::{
    CurrentDocument, CurrentInstanceRow, CurrentInstanceValue, CurrentSchemaCas,
    CurrentTypeSchemaRow, DependencyClosure, DependencyStore, EntityRow, EntityStore,
    EntityWriteOrderStore, InstanceStore, NewCurrentInstance, NewCurrentTypeSchema, NewEntity,
    NewInstanceRevision, NewOperation, NewOperationItem, NewRevision, OperationItemRow,
    OperationRow, OperationStore, ReverseImpact, TypeSchemaStore, VersionFamilyRow,
    VersionFamilyStore,
};

use super::repo::{
    CoordinationStateRepo, DependencyRepo, EntityRepo, InstanceRepo, OperationRepo, TypeSchemaRepo,
    VersionFamilyRepo,
};

/// The database-backed implementation of every port. Stateless, so it costs
/// nothing to construct and can be shared as an `Arc`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Repos;

#[async_trait]
impl EntityWriteOrderStore for Repos {
    async fn claim_entity_write_order(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        now: OffsetDateTime,
    ) -> Result<(), ScopeError> {
        CoordinationStateRepo::claim_entity_write_order(tx, scope, now).await
    }
}

#[async_trait]
impl VersionFamilyStore for Repos {
    async fn create_or_get(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        family_key: &FamilyKey,
        ownership_scope: OwnershipScope,
        owner_tenant_id: Option<Uuid>,
        now: OffsetDateTime,
    ) -> Result<(VersionFamilyRow, bool), ScopeError> {
        VersionFamilyRepo::create_or_get(
            tx,
            scope,
            family_key.as_str(),
            ownership_scope,
            owner_tenant_id,
            now,
        )
        .await
    }
}

#[async_trait]
impl EntityStore for Repos {
    async fn find_by_gts_id(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        gts_id: &str,
    ) -> Result<Option<EntityRow>, ScopeError> {
        EntityRepo::find_by_gts_id(tx, scope, gts_id).await
    }

    async fn find_by_gts_ids(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        gts_ids: &[String],
    ) -> Result<Vec<EntityRow>, ScopeError> {
        EntityRepo::find_by_gts_ids(tx, scope, gts_ids).await
    }

    async fn find_by_gts_uuid(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        gts_uuid: Uuid,
    ) -> Result<Option<EntityRow>, ScopeError> {
        EntityRepo::find_by_gts_uuid(tx, scope, gts_uuid).await
    }

    async fn kind_in_family(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        family_id: i64,
    ) -> Result<Option<EntityKind>, ScopeError> {
        EntityRepo::kind_in_family(tx, scope, family_id).await
    }

    async fn insert_entity(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        new: NewEntity,
    ) -> Result<Option<EntityRow>, ScopeError> {
        EntityRepo::insert(tx, scope, new).await
    }

    async fn compare_and_swap_version(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        entity_id: i64,
        expected_resource_version: i64,
        now: OffsetDateTime,
    ) -> Result<Option<i64>, ScopeError> {
        EntityRepo::compare_and_swap_version(tx, scope, entity_id, expected_resource_version, now)
            .await
    }
}

#[async_trait]
impl TypeSchemaStore for Repos {
    async fn current_documents(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        entity_ids: &[i64],
    ) -> Result<Vec<CurrentDocument>, ScopeError> {
        TypeSchemaRepo::current_documents(tx, scope, entity_ids).await
    }

    async fn find_current_schema(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        entity_id: i64,
    ) -> Result<Option<CurrentTypeSchemaRow>, ScopeError> {
        TypeSchemaRepo::find_current(tx, scope, entity_id).await
    }

    async fn current_schemas(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        entity_ids: &[i64],
    ) -> Result<Vec<CurrentTypeSchemaRow>, ScopeError> {
        TypeSchemaRepo::current_states(tx, scope, entity_ids).await
    }

    async fn insert_schema_revision(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        new: NewRevision,
    ) -> Result<(), ScopeError> {
        TypeSchemaRepo::insert_revision(tx, scope, new).await
    }

    async fn insert_current_schema(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        new: NewCurrentTypeSchema,
    ) -> Result<(), ScopeError> {
        TypeSchemaRepo::insert_current(tx, scope, new).await
    }

    async fn update_current_schema(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        new: NewCurrentTypeSchema,
        expected: CurrentSchemaCas,
    ) -> Result<bool, ScopeError> {
        TypeSchemaRepo::update_current(tx, scope, new, expected).await
    }
}

#[async_trait]
impl InstanceStore for Repos {
    async fn current_values(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        entity_ids: &[i64],
    ) -> Result<Vec<CurrentInstanceValue>, ScopeError> {
        InstanceRepo::current_values(tx, scope, entity_ids).await
    }

    async fn find_current_instance(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        entity_id: i64,
    ) -> Result<Option<CurrentInstanceRow>, ScopeError> {
        InstanceRepo::find_current(tx, scope, entity_id).await
    }

    async fn insert_instance_revision(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        new: NewInstanceRevision,
    ) -> Result<(), ScopeError> {
        InstanceRepo::insert_revision(tx, scope, new).await
    }

    async fn insert_current_instance(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        new: NewCurrentInstance,
    ) -> Result<(), ScopeError> {
        InstanceRepo::insert_current(tx, scope, new).await
    }

    async fn update_current_instance(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        new: NewCurrentInstance,
    ) -> Result<bool, ScopeError> {
        InstanceRepo::update_current(tx, scope, new).await
    }
}

#[async_trait]
impl OperationStore for Repos {
    async fn find_by_idempotency(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        idempotency_scope_hash: &ScopeHash,
        idempotency_key: &str,
    ) -> Result<Option<OperationRow>, ScopeError> {
        OperationRepo::find_by_idempotency(
            tx,
            scope,
            idempotency_scope_hash.as_bytes(),
            idempotency_key,
        )
        .await
    }

    async fn find_by_id(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        id: Uuid,
    ) -> Result<Option<OperationRow>, ScopeError> {
        OperationRepo::find_by_id(tx, scope, id).await
    }

    async fn insert_operation(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        new: NewOperation,
    ) -> Result<OperationRow, ScopeError> {
        OperationRepo::insert(tx, scope, new).await
    }

    async fn insert_items(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        parent: &OperationRow,
        items: &[NewOperationItem],
    ) -> Result<(), ScopeError> {
        OperationRepo::insert_items(tx, scope, parent, items).await
    }

    async fn find_items(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        operation_id: Uuid,
    ) -> Result<Vec<OperationItemRow>, ScopeError> {
        OperationRepo::find_items(tx, scope, operation_id).await
    }

    async fn mark_running(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        id: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, ScopeError> {
        OperationRepo::mark_running(tx, scope, id, now).await
    }

    async fn mark_completed(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        id: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, ScopeError> {
        OperationRepo::mark_completed(tx, scope, id, now).await
    }

    async fn mark_item_succeeded(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        item_id: i64,
        revision_no: i32,
        resource_version: i64,
        now: OffsetDateTime,
    ) -> Result<bool, ScopeError> {
        OperationRepo::mark_item_succeeded(tx, scope, item_id, revision_no, resource_version, now)
            .await
    }

    async fn mark_item_unchanged(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        item_id: i64,
        resource_version: i64,
        now: OffsetDateTime,
    ) -> Result<bool, ScopeError> {
        OperationRepo::mark_item_unchanged(tx, scope, item_id, resource_version, now).await
    }

    async fn mark_item_failed(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        item_id: i64,
        error_payload: String,
        now: OffsetDateTime,
    ) -> Result<bool, ScopeError> {
        OperationRepo::mark_item_failed(tx, scope, item_id, error_payload, now).await
    }
}

#[async_trait]
impl DependencyStore for Repos {
    async fn closure(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        roots: &[String],
    ) -> Result<DependencyClosure, ScopeError> {
        DependencyRepo::closure(tx, scope, roots).await
    }

    async fn reverse_impact(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        roots: &[i64],
        write_set_bound: usize,
    ) -> Result<ReverseImpact, ScopeError> {
        DependencyRepo::reverse_impact(tx, scope, roots, write_set_bound).await
    }

    async fn replace_outgoing(
        &self,
        tx: &DbTx<'_>,
        scope: &AccessScope,
        from_entity_id: i64,
        edges: &[(DependencyKind, i64)],
    ) -> Result<(), ScopeError> {
        DependencyRepo::replace_outgoing(tx, scope, from_entity_id, edges).await
    }
}
