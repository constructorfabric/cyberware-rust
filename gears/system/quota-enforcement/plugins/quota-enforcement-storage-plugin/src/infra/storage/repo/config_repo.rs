//! Configuration tables: idempotent seeding of the platform-default rows.
//!
//! Every insert is insert-if-absent. A primary-key violation means a peer
//! replica seeded the row first, which is the same end state.

use quota_enforcement_sdk::ConfigDefaults;
use sea_orm::{ActiveValue, EntityTrait};

use crate::domain::ports::SeedReport;
use time::OffsetDateTime;
use toolkit_db::secure::{
    DBRunner, ScopeError, SecureEntityExt, is_unique_violation, secure_insert,
};
use toolkit_security::AccessScope;

use crate::infra::storage::entity::{
    DEFAULT_KEY, contention_timeout_config, idempotency_retention_config, lease_capacity_config,
};

/// A default value does not fit its column type.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("configuration default {field}={value} does not fit the column type")]
pub struct DefaultOutOfRange {
    /// The `ConfigDefaults` field.
    pub field: &'static str,
    /// The offending value.
    pub value: u64,
}

/// Failure of `seed_defaults`.
#[derive(Debug, thiserror::Error)]
pub enum SeedError {
    /// A default does not fit its column.
    #[error(transparent)]
    OutOfRange(#[from] DefaultOutOfRange),
    /// The database rejected a read or write.
    #[error(transparent)]
    Db(#[from] ScopeError),
}

/// Seed the three platform-default rows when missing.
///
/// # Errors
///
/// - [`SeedError::OutOfRange`] when a default exceeds its column type.
/// - [`SeedError::Db`] on any database failure other than a primary-key
///   violation.
pub async fn seed_defaults(
    runner: &impl DBRunner,
    defaults: &ConfigDefaults,
) -> Result<SeedReport, SeedError> {
    let timeout_ms =
        i64::try_from(defaults.contention_timeout_ms).map_err(|_| DefaultOutOfRange {
            field: "contention_timeout_ms",
            value: defaults.contention_timeout_ms,
        })?;
    let max_active_leases =
        i32::try_from(defaults.max_active_leases).map_err(|_| DefaultOutOfRange {
            field: "max_active_leases",
            value: u64::from(defaults.max_active_leases),
        })?;
    let retention_seconds =
        i64::try_from(defaults.idempotency_retention_secs).map_err(|_| DefaultOutOfRange {
            field: "idempotency_retention_secs",
            value: defaults.idempotency_retention_secs,
        })?;
    let now = OffsetDateTime::now_utc();
    let key = || ActiveValue::Set(DEFAULT_KEY.to_owned());

    let mut report = SeedReport::default();
    let scope = AccessScope::allow_all();

    let inserted = insert_if_absent::<contention_timeout_config::Entity>(
        runner,
        &scope,
        contention_timeout_config::ActiveModel {
            metric_key: key(),
            timeout_ms: ActiveValue::Set(timeout_ms),
            updated_at: ActiveValue::Set(now),
        },
    )
    .await?;
    report.count(inserted);

    let inserted = insert_if_absent::<lease_capacity_config::Entity>(
        runner,
        &scope,
        lease_capacity_config::ActiveModel {
            tenant_key: key(),
            metric_key: key(),
            max_active_leases: ActiveValue::Set(max_active_leases),
            updated_at: ActiveValue::Set(now),
        },
    )
    .await?;
    report.count(inserted);

    let inserted = insert_if_absent::<idempotency_retention_config::Entity>(
        runner,
        &scope,
        idempotency_retention_config::ActiveModel {
            tenant_key: key(),
            metric_key: key(),
            retention_seconds: ActiveValue::Set(retention_seconds),
            updated_at: ActiveValue::Set(now),
        },
    )
    .await?;
    report.count(inserted);

    Ok(report)
}

async fn insert_if_absent<E>(
    runner: &impl DBRunner,
    scope: &AccessScope,
    row: E::ActiveModel,
) -> Result<bool, ScopeError>
where
    E: EntityTrait + toolkit_db::secure::ScopableEntity,
    E::Model: sea_orm::IntoActiveModel<E::ActiveModel> + Send + Sync,
    E::ActiveModel: sea_orm::ActiveModelBehavior + Send + Sync,
{
    match secure_insert::<E>(row, scope, runner).await {
        Ok(_) => Ok(true),
        Err(ScopeError::Db(db)) if is_unique_violation(&db) => Ok(false),
        Err(err) => Err(err),
    }
}

/// The platform-default contention timeout, if seeded.
///
/// # Errors
///
/// Returns the database error of the read.
pub async fn read_default_contention_timeout(
    runner: &impl DBRunner,
) -> Result<Option<i64>, ScopeError> {
    let row = contention_timeout_config::Entity::find_by_id(DEFAULT_KEY.to_owned())
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(runner)
        .await?;
    Ok(row.map(|r| r.timeout_ms))
}

/// The platform-default active-lease cap, if seeded.
///
/// # Errors
///
/// Returns the database error of the read.
pub async fn read_default_lease_capacity(
    runner: &impl DBRunner,
) -> Result<Option<i32>, ScopeError> {
    let row =
        lease_capacity_config::Entity::find_by_id((DEFAULT_KEY.to_owned(), DEFAULT_KEY.to_owned()))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .one(runner)
            .await?;
    Ok(row.map(|r| r.max_active_leases))
}

/// The platform-default idempotency retention, if seeded.
///
/// # Errors
///
/// Returns the database error of the read.
pub async fn read_default_idempotency_retention(
    runner: &impl DBRunner,
) -> Result<Option<i64>, ScopeError> {
    let row = idempotency_retention_config::Entity::find_by_id((
        DEFAULT_KEY.to_owned(),
        DEFAULT_KEY.to_owned(),
    ))
    .secure()
    .scope_with(&AccessScope::allow_all())
    .one(runner)
    .await?;
    Ok(row.map(|r| r.retention_seconds))
}
