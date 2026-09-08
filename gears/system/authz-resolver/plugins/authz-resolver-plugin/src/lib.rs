//! `AuthZ` Resolver Plugin — the platform's policy decision point.
//!
//! Implements the Constructor Fabric `AuthZResolverPluginClient` trait as an
//! in-process Policy Decision Point (PDP), discoverable through the GTS
//! Schema Registry and `ClientHub`. `evaluate()` runs the documented 8-step
//! pipeline: request validation → GTS type validation → scope enforcement →
//! policy evaluation (RBAC) → scope materialization → `require_constraints`
//! branch → constraint generation → audit emission. Every `Ok(_)` return
//! emits an audit record; `Err(_)` returns skip audit. Fail-closed
//! throughout — system errors surface as `Err`; business denials surface
//! as `Ok(decision=false)`.
//!
//! Enabling the `test-support` feature pulls in `test_support` (in-memory
//! fakes + a metrics harness). Production binaries leave it off.

pub mod config;
pub mod module;

// `#[doc(hidden)] pub` rather than `pub(crate)` — same shape as the `rbac`
// gear. The internals are not part of the crate contract (only `config`,
// `module`, and the re-export below are), but declaring the modules
// `pub(crate)` makes every `pub(crate)` item inside them redundant, which
// the workspace `clippy::redundant_pub_crate` lint denies.
#[doc(hidden)]
pub mod domain;
#[doc(hidden)]
pub mod infra;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use module::AuthZResolverPluginGear;
