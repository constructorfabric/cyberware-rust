//! Repository for the per-tenant invoice-posting policy
//! (`ledger_tenant_posting_policy`, VHP-1853): resolve the version in effect
//! (latest `effective_from <= at`, highest `version` on a tie) and append a new
//! effective-dated version. Mirrors `PaymentRepo::read_effective_policy`
//! (precedence) + `ApprovalRepo::insert_policy_row` (dual-control). Tenant-scoped
//! via `SecureORM` (SQL-level BOLA); out-of-txn on a fresh scoped connection (the
//! policy is admin-plane, never the hot money path).

use sea_orm::ActiveValue::Set;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order};
use toolkit_db::secure::{AccessScope, SecureEntityExt, SecureInsertExt};
use toolkit_db::{DBProvider, DbError};
use uuid::Uuid;

use crate::domain::invoice::policy::{AgingThresholds, MissingMappingMode, PostingPolicy};
use crate::domain::model::RepoError;
use crate::infra::storage::entity::posting_policy;
use time::OffsetDateTime;

/// `SeaORM`-backed posting-policy repository.
#[derive(Clone)]
pub struct PostingPolicyRepo {
    db: DBProvider<DbError>,
}

impl PostingPolicyRepo {
    /// Build over one database provider.
    #[must_use]
    pub fn new(db: DBProvider<DbError>) -> Self {
        Self { db }
    }

    /// Resolve the posting policy in effect for `tenant` at instant `at` — the
    /// row with the latest `effective_from <= at` (ties: highest `version`).
    /// Returns the gear default (`Suspense` + `[30,60,90]`, the prior hardcoded
    /// behaviour) when the tenant has no effective row, so the caller always gets
    /// an applicable policy. SQL-level BOLA: a foreign tenant yields no rows.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope / storage failure, or when a stored value
    /// fails to parse (an invariant breach — the columns are CHECK-constrained
    /// and only written via the validated domain types).
    pub async fn read_effective_policy(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        at: OffsetDateTime,
    ) -> Result<PostingPolicy, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        let row = posting_policy::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(posting_policy::Column::TenantId.eq(tenant))
                    .add(posting_policy::Column::EffectiveFrom.lte(at)),
            )
            .order_by(posting_policy::Column::EffectiveFrom, Order::Desc)
            .order_by(posting_policy::Column::Version, Order::Desc)
            .one(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("read posting policy: {e}")))?;
        let Some(row) = row else {
            return Ok(PostingPolicy::default());
        };
        let missing_mapping_mode =
            MissingMappingMode::parse(&row.missing_mapping_mode).map_err(|e| {
                RepoError::Db(format!(
                    "corrupt missing_mapping_mode for tenant {tenant} version {}: {e}",
                    row.version
                ))
            })?;
        let aging_thresholds =
            AgingThresholds::parse_csv(&row.ar_aging_thresholds).map_err(|e| {
                RepoError::Db(format!(
                    "corrupt ar_aging_thresholds for tenant {tenant} version {}: {e}",
                    row.version
                ))
            })?;
        Ok(PostingPolicy {
            missing_mapping_mode,
            aging_thresholds,
        })
    }

    /// Append a new effective-dated policy version for `tenant`, effective from
    /// `effective_from`. The version is `max(version) + 1` (`0` for the first).
    /// Returns the new version. SQL-level BOLA. The `(tenant, version)` PK guards
    /// a concurrent double-write (the loser fails with a storage error — accepted
    /// for the rare admin-plane write).
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope / storage failure.
    pub async fn write_version(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        policy: &PostingPolicy,
        effective_from: OffsetDateTime,
    ) -> Result<i64, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        let current = posting_policy::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(Condition::all().add(posting_policy::Column::TenantId.eq(tenant)))
            .order_by(posting_policy::Column::Version, Order::Desc)
            .one(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("read max posting-policy version: {e}")))?;
        let version = current.map_or(0, |r| r.version + 1);
        let am = posting_policy::ActiveModel {
            tenant_id: Set(tenant),
            version: Set(version),
            effective_from: Set(effective_from),
            missing_mapping_mode: Set(policy.missing_mapping_mode.as_str().to_owned()),
            ar_aging_thresholds: Set(policy.aging_thresholds.to_csv()),
            created_at_utc: Set(OffsetDateTime::now_utc()),
        };
        posting_policy::Entity::insert(am.clone())
            .secure()
            .scope_with_model(scope, &am)
            .map_err(|e| RepoError::Db(format!("ledger_tenant_posting_policy scope: {e}")))?
            .exec(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("insert ledger_tenant_posting_policy: {e}")))?;
        Ok(version)
    }
}
