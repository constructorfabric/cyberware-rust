//! Helpers shared across the `repo_impl` split: the repository adapter type,
//! entity/domain converters, and error mapping. Kept in one leaf module so
//! `reads`/`writes` and the parent depend on it one-way (no module cycle).

use std::sync::Arc;

use credstore_sdk::{OwnerId, SharingMode, TenantId};
use toolkit_db::DBProvider;
use toolkit_db::secure::ScopeError;

use crate::domain::error::DomainError;
use crate::domain::secret::model::{SecretRow, SecretStatus};
use crate::infra::canonical_mapping::classify_db_err_to_domain;
use crate::infra::storage::entity;

pub type CredstoreDbProvider = DBProvider<DomainError>;

/// `SeaORM` repository adapter for
/// [`SecretRepo`](crate::domain::secret::repo::SecretRepo).
pub struct SecretRepoImpl {
    pub(crate) db: Arc<CredstoreDbProvider>,
}

impl SecretRepoImpl {
    #[must_use]
    pub fn new(db: Arc<CredstoreDbProvider>) -> Self {
        Self { db }
    }
}

/// Map an entity row to the domain [`SecretRow`].
pub(crate) fn entity_to_model(m: entity::secrets::Model) -> Result<SecretRow, DomainError> {
    let sharing = sharing_from_i16(m.sharing).ok_or_else(|| DomainError::Internal {
        diagnostic: format!(
            "credstore_secrets.sharing out-of-domain value: {}",
            m.sharing
        ),
        cause: None,
    })?;
    let status = SecretStatus::from_smallint(m.status).ok_or_else(|| DomainError::Internal {
        diagnostic: format!("credstore_secrets.status out-of-domain value: {}", m.status),
        cause: None,
    })?;
    Ok(SecretRow {
        id: m.id,
        tenant_id: TenantId(m.tenant_id),
        reference: m.reference,
        sharing,
        owner_id: OwnerId(m.owner_id),
        status,
        version: m.version,
        // Opaque here: the domain layer resolves the UUID to the type id +
        // traits via the types-registry, so non-catalog types round-trip.
        secret_type_uuid: m.secret_type_uuid,
        expires_at: m.expires_at,
        value_fp: m.value_fp,
        fp_key_id: m.fp_key_id,
    })
}

/// Map [`SharingMode`] to its `SMALLINT` storage value.
pub(crate) fn sharing_to_i16(s: SharingMode) -> i16 {
    match s {
        SharingMode::Private => 1,
        SharingMode::Tenant => 2,
        SharingMode::Shared => 3,
    }
}

/// Map a `SMALLINT` storage value to [`SharingMode`].
pub(crate) fn sharing_from_i16(v: i16) -> Option<SharingMode> {
    match v {
        1 => Some(SharingMode::Private),
        2 => Some(SharingMode::Tenant),
        3 => Some(SharingMode::Shared),
        _ => None,
    }
}

/// Map a [`ScopeError`] to a [`DomainError`] outside a retry boundary.
pub(super) fn map_scope_err(err: ScopeError) -> DomainError {
    match err {
        ScopeError::Db(db) => classify_db_err_to_domain(db),
        ScopeError::Invalid(msg) => DomainError::Internal {
            diagnostic: format!("scope invalid: {msg}"),
            cause: None,
        },
        ScopeError::TenantNotInScope { .. } => DomainError::AccessDenied { cause: None },
        ScopeError::Denied(msg) => DomainError::Internal {
            diagnostic: format!("unexpected access denied in credstore repo: {msg}"),
            cause: None,
        },
        // `ScopeError` is `#[non_exhaustive]`: variants this gear has no
        // specific answer for (today the graph-query refusals, which it can
        // never trigger) map to an internal error, like `Invalid`.
        other => DomainError::Internal {
            diagnostic: format!("scope invalid: {other}"),
            cause: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::DbErr;
    use toolkit_db::secure::ScopeError;
    use uuid::Uuid;

    use super::map_scope_err;
    use crate::domain::error::DomainError;

    #[test]
    fn maps_each_scope_error_variant() {
        assert!(matches!(
            map_scope_err(ScopeError::Invalid("bad scope")),
            DomainError::Internal { .. }
        ));
        assert!(matches!(
            map_scope_err(ScopeError::TenantNotInScope {
                tenant_id: Uuid::new_v4()
            }),
            DomainError::AccessDenied { .. }
        ));
        assert!(matches!(
            map_scope_err(ScopeError::Denied("not accessible")),
            DomainError::Internal { .. }
        ));
        // Db errors delegate to the classification ladder (CHECK violations
        // are server-side invariants → Internal).
        assert!(matches!(
            map_scope_err(ScopeError::Db(DbErr::Custom(
                "CHECK constraint failed".to_owned()
            ))),
            DomainError::Internal { .. }
        ));
    }
}
