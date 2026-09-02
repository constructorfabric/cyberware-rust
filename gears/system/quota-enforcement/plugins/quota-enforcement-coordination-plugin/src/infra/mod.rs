//! Infrastructure: the lock service on `qe_coordination_locks`, the entity,
//! and the migration. The SDK trait is the contract; everything here is the
//! database realization of it.

pub mod lock_service;
pub mod storage;

pub use lock_service::DbCoordination;
