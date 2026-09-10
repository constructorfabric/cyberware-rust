//! The `version_family` repository: the row that names a version family and, by
//! its unique key, serializes concurrent first admission into it.

use sea_orm::{ActiveValue::Set, ColumnTrait, DbErr, EntityTrait, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::secure::{AccessScope, DBRunner, ScopeError, SecureEntityExt, SecureInsertExt};
use uuid::Uuid;

use super::conflict_do_nothing;
use crate::domain::enums::OwnershipScope;
use crate::domain::family::FamilyKey;
use crate::domain::ports::VersionFamilyRow;
use crate::infra::storage::entity::version_family;

/// One stored row as the domain names it. See `entity_repo::row` for why the
/// mapper sits beside the repository rather than on the entity.
fn row(m: version_family::Model) -> VersionFamilyRow {
    VersionFamilyRow {
        id: m.id,
        family_key: FamilyKey::from_stored(m.family_key),
        ownership_scope: m.ownership_scope.into(),
        owner_tenant_id: m.owner_tenant_id,
        created_at: m.created_at,
    }
}

pub struct VersionFamilyRepo;

impl VersionFamilyRepo {
    /// Exact read by family key.
    ///
    /// # Errors
    /// Propagates scope validation and database query failures.
    pub async fn find_by_key(
        runner: &impl DBRunner,
        scope: &AccessScope,
        family_key: &str,
    ) -> Result<Option<VersionFamilyRow>, ScopeError> {
        Ok(version_family::Entity::find()
            .filter(version_family::Column::FamilyKey.eq(family_key))
            .secure()
            .scope_with(scope)
            .one(runner)
            .await?
            .map(row))
    }

    /// Insert the family if absent, otherwise read the existing row. Returns
    /// `(row, created)`.
    ///
    /// The winner is decided by `uq_tr_version_family_key`, not by the read that
    /// precedes the insert: two callers can both see no row, and the constraint
    /// keeps the loser's out. The conflict is the serialization point, not a fault,
    /// so it is handled — and it must be *absorbed*, not raised, because this runs
    /// inside the admission commit transaction and a raised unique violation aborts
    /// that transaction on `PostgreSQL`. See [`conflict_do_nothing`].
    ///
    /// Family validation is serialized by the transaction's `entity_write_order`
    /// claim, not by a family-row lock (SPEC §8.1).
    ///
    /// # Errors
    /// Propagates scope validation and database failures. Returns
    /// [`ScopeError::Invalid`] if the winning family disappears before re-read.
    pub async fn create_or_get(
        runner: &impl DBRunner,
        scope: &AccessScope,
        family_key: &str,
        ownership_scope: OwnershipScope,
        owner_tenant_id: Option<Uuid>,
        now: OffsetDateTime,
    ) -> Result<(VersionFamilyRow, bool), ScopeError> {
        if let Some(existing) = Self::find_by_key(runner, scope, family_key).await? {
            return Ok((existing, false));
        }

        let candidate = version_family::ActiveModel {
            family_key: Set(family_key.to_owned()),
            ownership_scope: Set(ownership_scope.into()),
            owner_tenant_id: Set(owner_tenant_id),
            created_at: Set(now),
            ..Default::default()
        };
        // `exec` rather than `exec_with_returning`: it is the one execution shape
        // that reports a swallowed conflict as `RecordNotInserted` on all three
        // backends. `exec_with_returning` reports the same fact as `RecordNotFound`
        // wherever `RETURNING` is available, which would leave this matching on a
        // library's message text.
        let created = match version_family::Entity::insert(candidate.clone())
            .secure()
            .scope_with_model(scope, &candidate)?
            .on_conflict_raw(conflict_do_nothing(version_family::Column::Id))
            .exec(runner)
            .await
        {
            Ok(_) => true,
            Err(ScopeError::Db(DbErr::RecordNotInserted)) => false,
            Err(e) => return Err(e),
        };

        // One read for both outcomes: either this call's row or the winner's, and
        // the conflict left the transaction usable either way — which is the whole
        // point of absorbing it instead of raising it.
        let family = Self::find_by_key(runner, scope, family_key)
            .await?
            // The row exists by now, inserted here or by the winner. If the read
            // misses it, something outside this protocol deleted it, and failing
            // closed is the only safe answer.
            .ok_or(ScopeError::Invalid(
                "version family vanished between insert and re-read",
            ))?;
        Ok((family, created))
    }
}
