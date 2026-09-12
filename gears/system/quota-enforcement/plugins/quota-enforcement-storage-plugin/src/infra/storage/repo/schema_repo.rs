//! `qe_schema_meta` access: read and record the installed contract major.

use sea_orm::{ActiveValue, EntityTrait, QueryOrder};
use time::OffsetDateTime;
use toolkit_db::secure::{
    DBRunner, ScopeError, SecureEntityExt, is_unique_violation, secure_insert,
};
use toolkit_security::AccessScope;

use crate::infra::storage::entity::schema_meta;

/// The installed contract major, if the schema was ever bootstrapped.
///
/// When more than one row exists the lowest major wins: a second row can
/// only appear through an operator mistake, and the stricter reading fails
/// closed at bootstrap.
///
/// # Errors
///
/// Returns the database error of the read.
pub async fn read_installed_major(runner: &impl DBRunner) -> Result<Option<i32>, ScopeError> {
    let row = schema_meta::Entity::find()
        .order_by_asc(schema_meta::Column::ContractMajor)
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(runner)
        .await?;
    Ok(row.map(|r| r.contract_major))
}

/// Record `major` as the installed contract major.
///
/// Returns `true` when this call wrote the row and `false` when a concurrent
/// bootstrap wrote it first.
///
/// # Errors
///
/// Returns the database error of the insert, except a primary-key violation.
pub async fn record_major(runner: &impl DBRunner, major: i32) -> Result<bool, ScopeError> {
    let row = schema_meta::ActiveModel {
        contract_major: ActiveValue::Set(major),
        applied_at: ActiveValue::Set(OffsetDateTime::now_utc()),
    };
    match secure_insert::<schema_meta::Entity>(row, &AccessScope::allow_all(), runner).await {
        Ok(_) => Ok(true),
        Err(ScopeError::Db(db)) if is_unique_violation(&db) => Ok(false),
        Err(err) => Err(err),
    }
}
