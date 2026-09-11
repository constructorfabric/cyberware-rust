//! Repository contracts (`DESIGN.md:595`'s `SubscriptionRepo`, `CursorRepo`).
//! Signatures only - no implementation (backed by `infra::storage::Storage`
//! for persisted entities, `domain::cluster` for ephemeral cache-backed ones
//! per the Domain Model's persisted-vs-ephemeral split). `TopicRepo` and
//! `EventRepo` used to live here but are gone
//! (eb-single-process-implementation D1/D3): topics go through
//! `SpecificationManager` exclusively, and events go straight to the
//! resolved `event_broker_sdk::EventBrokerBackend` (`domain::backend::
//! BackendResolver`) - no repo-level indirection for either.
//!
//! Method names are deliberately distinct across these traits
//! (`find_subscription`, `find_cursor`, `find_consumer_group`, not a shared
//! `get`) - `Storage` (`infra/storage/storage.rs`) implements all of them on
//! one struct, and identically-named methods across traits in scope on the
//! same receiver are ambiguous at the call site (would force
//! `Trait::method(&*repo, ..)` UFCS everywhere instead of `repo.method(..)`).

use async_trait::async_trait;
use toolkit_gts::GtsInstanceId;

use crate::domain::error::DomainError;
use crate::domain::model::{ConsumerGroup, Cursor, Subscription};

#[async_trait]
pub trait SubscriptionRepo: Send + Sync {
    async fn find_subscription(&self, id: uuid::Uuid) -> Result<Option<Subscription>, DomainError>;
    async fn list_subscriptions(&self) -> Result<Vec<Subscription>, DomainError>;
    async fn put_subscription(&self, subscription: &Subscription) -> Result<(), DomainError>;
    async fn delete_subscription(&self, id: uuid::Uuid) -> Result<(), DomainError>;
}

#[async_trait]
pub trait CursorRepo: Send + Sync {
    async fn find_cursor(
        &self,
        consumer_group: &GtsInstanceId,
        topic: &GtsInstanceId,
        partition: i32,
    ) -> Result<Option<Cursor>, DomainError>;

    async fn put_cursor(&self, cursor: &Cursor) -> Result<(), DomainError>;
}

#[async_trait]
pub trait ConsumerGroupRepo: Send + Sync {
    async fn create_consumer_group(
        &self,
        group: ConsumerGroup,
    ) -> Result<ConsumerGroup, DomainError>;
    async fn find_consumer_group(
        &self,
        id: &GtsInstanceId,
    ) -> Result<Option<ConsumerGroup>, DomainError>;
    #[toolkit_macros::temporary(
        tracking = "gears-rust#4347",
        reason = "no limit/cursor/filter params - callers must fetch every \
                  registered consumer group into memory and paginate/filter \
                  client-side; needs pagination pushdown once a real backend \
                  replaces the in-memory store"
    )]
    async fn list_consumer_groups(&self) -> Result<Vec<ConsumerGroup>, DomainError>;
    async fn delete_consumer_group(&self, id: &GtsInstanceId) -> Result<(), DomainError>;

    /// Whether any subscription currently has an active membership in this
    /// group - guards `DELETE /v1/consumer-groups/{id}`
    /// (`ConsumerGroupHasActiveMembers`).
    async fn has_active_members(&self, id: &GtsInstanceId) -> Result<bool, DomainError>;
}
