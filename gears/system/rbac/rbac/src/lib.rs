//! VHP RBAC Service module
//!
//! `toolkit` module wiring: registers `dyn RbacServiceClientV1` in `ClientHub`,
//! runs Postgres migrations through `DatabaseCapability`, seeds built-in
//! roles, and exposes the `/rbac/v1` REST mount via `RestApiCapability`. The
//! public contract lives in the sibling `rbac-sdk` crate.

// Declared first so `#[macro_use]` makes `stub_impl!` visible to sibling
// `*_tests` modules below.
#[cfg(test)]
#[macro_use]
mod test_support;

#[doc(hidden)]
pub mod api;
#[doc(hidden)]
pub mod config;
#[doc(hidden)]
pub mod domain;
#[doc(hidden)]
pub mod infra;
#[doc(hidden)]
pub mod module;
// GTS-instance compile-time declarations — `inventory::submit!` side-effects only.
pub(crate) mod gts;
#[doc(hidden)]
pub mod odata;
