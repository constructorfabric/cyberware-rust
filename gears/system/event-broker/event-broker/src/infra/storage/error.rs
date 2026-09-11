//! `toolkit_db` error conversions into [`DomainError`] - kept in `infra/`
//! rather than `domain/error.rs` since `domain/` has no infra dependencies
//! (`domain/mod.rs`'s own invariant); Rust's orphan rule permits a
//! foreign-trait-for-local-type `impl` from anywhere in the crate.

use crate::domain::error::DomainError;

/// `toolkit_db::DbError` covers connection/config-level failures (bad DSN,
/// disabled feature, etc.) - always infrastructure, never a caller mistake,
/// so it maps uniformly to `StorageUnavailable` regardless of variant.
/// Required by `DBProvider<DomainError>`'s `E: From<DbError>` bound.
impl From<toolkit_db::DbError> for DomainError {
    fn from(err: toolkit_db::DbError) -> Self {
        DomainError::StorageUnavailable(err.to_string())
    }
}

/// `toolkit_db::secure::ScopeError` distinguishes access-denial from
/// infrastructure failure (`docs/toolkit_unified_system/
/// 06_authn_authz_secure_orm.md`: "typically 403 for denied, 500 for DB
/// errors") - `Denied`/`TenantNotInScope` are PEP-adjacent scope violations
/// (a bug in how this gear compiled/passed the `AccessScope`, not a client
/// input error), so they map to `Forbidden` with a generic code; `Db`/
/// `Invalid` are infrastructure and map to `StorageUnavailable`.
impl From<toolkit_db::secure::ScopeError> for DomainError {
    fn from(err: toolkit_db::secure::ScopeError) -> Self {
        use toolkit_db::secure::ScopeError;
        match err {
            ScopeError::Denied(reason) => DomainError::Forbidden {
                code: "ScopeDenied",
                message: reason.to_owned(),
                resource: String::new(),
            },
            ScopeError::TenantNotInScope { tenant_id } => DomainError::Forbidden {
                code: "ScopeDenied",
                message: format!("tenant {tenant_id} not present in security scope"),
                resource: String::new(),
            },
            ScopeError::Db(_) | ScopeError::Invalid(_) => {
                DomainError::StorageUnavailable(err.to_string())
            }
            // `ScopeError` is `#[non_exhaustive]`; any variant added upstream is
            // an infrastructure-shaped failure until proven otherwise.
            _ => DomainError::StorageUnavailable(err.to_string()),
        }
    }
}

/// `cluster_sdk::ClusterError` covers resolution/backend failures for
/// `Storage`'s `subscription` namespace (`ClusterCacheV1`) - always
/// infrastructure, matching the `DbError` mapping above.
impl From<cluster_sdk::ClusterError> for DomainError {
    fn from(err: cluster_sdk::ClusterError) -> Self {
        DomainError::StorageUnavailable(err.to_string())
    }
}

/// `toolkit_db::secure::ScopeError` -> `toolkit_db::DbError`, so a
/// transaction closure (fixed to `DBProvider<DbError>`'s error type, per
/// `toolkit_db::DBProvider::transaction`'s signature) can propagate a scope
/// violation through `?` like any other DB failure - a scope violation
/// inside one of this crate's own transactions would indicate a bug in its
/// own `AccessScope` usage, not a caller mistake, so folding it into the
/// same generic-failure path the closure already uses is appropriate.
/// Used by `infra::storage::Storage::check_and_enqueue`, which runs inside a
/// `DBProvider<DbError>`-typed transaction.
pub(crate) fn db_err_from_scope(err: &toolkit_db::secure::ScopeError) -> toolkit_db::DbError {
    toolkit_db::DbError::Other(anyhow::anyhow!("{err}"))
}
