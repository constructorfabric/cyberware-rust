//! API surface: the REST routes, the in-process manager client, and the
//! readiness health check.
//!
//! Every REST operation is registered through
//! [`rest::routes::register_routes`]; it and the in-process client enter the
//! domain through the same service, so both transports share one
//! authorization boundary ([`crate::domain::Admission`]).

pub mod healthcheck;
pub mod in_process;
pub mod rest;
