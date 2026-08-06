//! Shared REST handler state: the domain services every ingest/delivery
//! handler needs, injected once via `Extension<Arc<HandlerState>>` after
//! route registration - the same "attach service once after all routes are
//! registered" convention `DispatcherState` follows
//! (`infra/dispatcher/forward.rs`, `eb-dispatcher-routing`).

use std::sync::Arc;

use crate::domain::delivery::DeliveryService;
use crate::domain::ingest::IngestService;

#[derive(Clone)]
pub struct HandlerState {
    pub ingest: Arc<dyn IngestService>,
    pub delivery: Arc<dyn DeliveryService>,
}
