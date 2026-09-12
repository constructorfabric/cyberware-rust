//! Host/in-process bootstrap gear
//!
//! This gear provides logging initialization, signal handling,
//! and path utilities for host processes.
//!
//! Configuration types are now in the top-level `config` gear.

#[cfg(feature = "otel")]
pub(crate) mod log_correlation;
pub mod logging;
pub mod panic;
pub mod paths;
pub mod signals;

pub use logging::*;
pub use panic::*;
pub use paths::{HomeDirError, expand_tilde, normalize_path};
pub use signals::*;
