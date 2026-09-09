use chrono::Utc;
use sea_orm::entity::prelude::*;
use sea_orm::{ActiveValue, ColumnTrait, EntityTrait, QueryFilter};
use toolkit_db::secure::{
    AccessScope, ScopableEntity, ScopeError, SecureEntityExt, SecureUpdateExt, secure_insert,
};

use crate::api::ProducerMode;
use crate::error::EventBrokerError;
use crate::ids::ProducerId;

use super::types::ManagedDeduplication;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "event_broker_producer_registrations")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub registration_key: String,
    pub producer_id: uuid::Uuid,
    pub mode: String,
    pub client_agent: String,
    pub generation: i64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

impl ScopableEntity for Entity {
    const IS_UNRESTRICTED: bool = true;

    fn tenant_col() -> Option<Self::Column> {
        None
    }

    fn resource_col() -> Option<Self::Column> {
        None
    }

    fn owner_col() -> Option<Self::Column> {
        None
    }

    fn type_col() -> Option<Self::Column> {
        None
    }

    fn resolve_property(_property: &str) -> Option<Self::Column> {
        None
    }
    fn scope_columns() -> Vec<Self::Column> {
        Vec::new()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ProducerRegistration {
    pub(crate) key: String,
    pub(crate) producer_id: ProducerId,
    pub(crate) mode: ProducerMode,
    pub(crate) client_agent: String,
    pub(crate) generation: i64,
}

#[derive(Clone)]
pub(crate) struct ProducerRegistrationStore {
    db: toolkit_db::Db,
}

impl ProducerRegistrationStore {
    pub(crate) fn new(db: toolkit_db::Db) -> Self {
        Self { db }
    }

    pub(crate) async fn load(
        &self,
        key: &str,
    ) -> Result<Option<ProducerRegistration>, EventBrokerError> {
        let conn = self.db.conn().map_err(map_db_err)?;
        let row = Entity::find()
            .filter(Column::RegistrationKey.eq(key))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .one(&conn)
            .await
            .map_err(map_scope_err)?;
        row.map(ProducerRegistration::try_from).transpose()
    }

    pub(crate) async fn insert_new(
        &self,
        managed: &ManagedDeduplication,
        producer_id: ProducerId,
        client_agent: &str,
    ) -> Result<ProducerRegistration, EventBrokerError> {
        let now = now_string();
        let registration = ProducerRegistration {
            key: managed.key.clone(),
            producer_id,
            mode: managed.mode,
            client_agent: client_agent.to_owned(),
            generation: 1,
        };
        let am = ActiveModel {
            registration_key: ActiveValue::Set(registration.key.clone()),
            producer_id: ActiveValue::Set(registration.producer_id.0),
            mode: ActiveValue::Set(mode_to_str(registration.mode).to_owned()),
            client_agent: ActiveValue::Set(registration.client_agent.clone()),
            generation: ActiveValue::Set(registration.generation),
            created_at: ActiveValue::Set(now.clone()),
            updated_at: ActiveValue::Set(now),
        };
        let conn = self.db.conn().map_err(map_db_err)?;
        secure_insert::<Entity>(am, &AccessScope::allow_all(), &conn)
            .await
            .map_err(map_scope_err)?;
        Ok(registration)
    }

    pub(crate) async fn replace_with_new_generation(
        &self,
        current: &ProducerRegistration,
        producer_id: ProducerId,
    ) -> Result<ProducerRegistration, EventBrokerError> {
        let generation = current.generation + 1;
        let now = now_string();
        let conn = self.db.conn().map_err(map_db_err)?;
        let result = Entity::update_many()
            .col_expr(
                Column::ProducerId,
                sea_orm::sea_query::Expr::value(producer_id.0),
            )
            .col_expr(
                Column::Generation,
                sea_orm::sea_query::Expr::value(generation),
            )
            .col_expr(Column::UpdatedAt, sea_orm::sea_query::Expr::value(now))
            .filter(Column::RegistrationKey.eq(current.key.as_str()))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(&conn)
            .await
            .map_err(map_scope_err)?;
        if result.rows_affected != 1 {
            return Err(EventBrokerError::Internal(format!(
                "producer registration '{}' was not updated",
                current.key
            )));
        }
        Ok(ProducerRegistration {
            key: current.key.clone(),
            producer_id,
            mode: current.mode,
            client_agent: current.client_agent.clone(),
            generation,
        })
    }
}

impl ProducerRegistration {
    pub(crate) fn validate_matches(
        &self,
        managed: &ManagedDeduplication,
        client_agent: &str,
    ) -> Result<(), EventBrokerError> {
        if self.mode != managed.mode {
            return Err(EventBrokerError::InvalidProducerOptions {
                detail: format!(
                    "managed producer registration '{}' has mode {}, expected {}",
                    self.key,
                    mode_to_str(self.mode),
                    mode_to_str(managed.mode)
                ),
                instance: String::new(),
            });
        }
        if self.client_agent != client_agent {
            return Err(EventBrokerError::InvalidProducerOptions {
                detail: format!(
                    "managed producer registration '{}' has client_agent '{}', expected '{}'",
                    self.key, self.client_agent, client_agent
                ),
                instance: String::new(),
            });
        }
        Ok(())
    }
}

impl TryFrom<Model> for ProducerRegistration {
    type Error = EventBrokerError;

    fn try_from(model: Model) -> Result<Self, Self::Error> {
        Ok(Self {
            key: model.registration_key,
            producer_id: ProducerId(model.producer_id),
            mode: mode_from_str(&model.mode)?,
            client_agent: model.client_agent,
            generation: model.generation,
        })
    }
}

pub(crate) fn mode_to_str(mode: ProducerMode) -> &'static str {
    match mode {
        ProducerMode::Stateless => "stateless",
        ProducerMode::Monotonic => "monotonic",
        ProducerMode::Chained => "chained",
    }
}

fn mode_from_str(mode: &str) -> Result<ProducerMode, EventBrokerError> {
    match mode {
        "stateless" => Ok(ProducerMode::Stateless),
        "monotonic" => Ok(ProducerMode::Monotonic),
        "chained" => Ok(ProducerMode::Chained),
        other => Err(EventBrokerError::Internal(format!(
            "unknown persisted producer mode '{other}'"
        ))),
    }
}

fn now_string() -> String {
    Utc::now().to_rfc3339()
}

fn map_db_err(err: toolkit_db::DbError) -> EventBrokerError {
    EventBrokerError::Internal(format!("producer registration db access: {err}"))
}

fn map_scope_err(err: ScopeError) -> EventBrokerError {
    EventBrokerError::Internal(format!("producer registration store: {err}"))
}
