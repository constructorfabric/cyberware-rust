//! GTS-instance declarations for the RBAC module.
//!
//! Each declaration uses `gts_instance!` to emit an `inventory::submit!`;
//! `types-registry::init()` aggregates them at startup and validates each
//! payload against its declared schema.

pub mod permissions;
