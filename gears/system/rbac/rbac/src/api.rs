//! REST entry points for the RBAC module.
//!
//! Service-side operation handlers (one struct per HTTP operation)
//! live under [`service`]. The Axum wiring lives under [`rest`].

pub mod rest;
pub mod service;
