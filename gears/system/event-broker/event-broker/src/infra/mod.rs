//! Infrastructure layer: persistence, cluster coordination, and GTS
//! provisioning (`DESIGN.md` §1.3 Architecture Layers).

pub mod cluster;
pub mod dispatcher;
pub mod loader;
pub mod partition_cache;
pub mod specification;
pub mod storage;
pub mod wiring;
pub mod workers;

#[cfg(test)]
mod wiring_tests;
