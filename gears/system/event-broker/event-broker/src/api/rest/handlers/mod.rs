//! REST handlers, grouped per `DESIGN.md:578-591`'s module tree
//! (`eb-rest-handlers`, all 19 endpoints), split into `ingest`/`delivery`
//! submodules mirroring `domain::{ingest, delivery}`'s `IngestService`/
//! `DeliveryService` split - every handler function calls exactly one of
//! `state.ingest`/`state.delivery`, so the split is exact, not approximate.
//! `action_suffix` is the one shared exception: it's used by one handler on
//! each side (`ingest::producers::reset_producer`,
//! `delivery::subscriptions::seek_subscription`), so it stays at this level
//! rather than picking a side.

mod action_suffix;
pub mod delivery;
pub mod ingest;

#[cfg(test)]
#[path = "smoke_tests.rs"]
mod smoke_tests;
