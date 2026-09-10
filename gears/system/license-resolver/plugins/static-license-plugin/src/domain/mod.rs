//! Domain layer for the static license resolver plugin.

mod client;
pub mod service;

pub use service::{BACKEND_ID, Service, deny_reason, diagnostics};
