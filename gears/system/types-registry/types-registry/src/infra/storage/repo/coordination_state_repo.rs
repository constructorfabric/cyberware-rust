//! Purpose-specific access to installation-wide coordination state.
//!
//! State names stay private so callers cannot silently update a misspelled key.

use sea_orm::sea_query::{Expr, ExprTrait};
use sea_orm::{ColumnTrait, Condition, EntityTrait};
use time::OffsetDateTime;
use toolkit_db::secure::{AccessScope, DBRunner, ScopeError, SecureEntityExt, SecureUpdateExt};

use crate::infra::storage::entity::coordination_state;

/// The migration-seeded entity-write serialization row.
const ENTITY_WRITE_ORDER: &str = "entity_write_order";

pub struct CoordinationStateRepo;

impl CoordinationStateRepo {
    /// Claim the entity-write order as the transaction's first statement.
    ///
    /// # Errors
    /// Propagates the update's failure. [`ScopeError::Invalid`] if the seeded row
    /// is missing, which means `m20260904_000002_coordination_state` did not run.
    pub async fn claim_entity_write_order(
        runner: &impl DBRunner,
        scope: &AccessScope,
        now: OffsetDateTime,
    ) -> Result<(), ScopeError> {
        let result = coordination_state::Entity::update_many()
            .secure()
            .col_expr(
                coordination_state::Column::StateSeq,
                Expr::col(coordination_state::Column::StateSeq).add(1),
            )
            .col_expr(coordination_state::Column::UpdatedAt, Expr::value(now))
            .filter(
                Condition::all().add(coordination_state::Column::StateName.eq(ENTITY_WRITE_ORDER)),
            )
            .scope_with(scope)
            .exec(runner)
            .await?;
        if result.rows_affected == 1 {
            return Ok(());
        }
        Err(ScopeError::Invalid(
            "the entity_write_order state row is absent; its migration did not run",
        ))
    }

    /// Return the diagnostic count of entity-write claims.
    ///
    /// # Errors
    /// Propagates the query's failure. [`ScopeError::Invalid`] if the seeded row is
    /// missing.
    pub async fn entity_write_sequence(
        runner: &impl DBRunner,
        scope: &AccessScope,
    ) -> Result<i64, ScopeError> {
        coordination_state::Entity::find_by_id(ENTITY_WRITE_ORDER)
            .secure()
            .scope_with(scope)
            .one(runner)
            .await?
            .map(|row| row.state_seq)
            .ok_or(ScopeError::Invalid(
                "the entity_write_order state row is absent; its migration did not run",
            ))
    }
}
