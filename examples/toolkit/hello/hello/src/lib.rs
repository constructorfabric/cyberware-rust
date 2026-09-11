//! Hello - minimal self-contained REST `OoP` demo gear.
//!
//! Exposes a single anonymous, externally-exposed route:
//!
//! ```text
//! GET /hello/v1/ping  ->  { "message": "pong", "served_by": "hello-oop (pid N)" }
//! ```
//!
//! It has no dependencies on other gears, so it can run as its own pod in
//! Profile 3 (Kubernetes): it registers its REST endpoint with the platform
//! host's `DirectoryService`, and the api-gateway edge reverse-proxies external
//! `/hello/v1/ping` requests to this pod. `served_by` reports the serving
//! process id so a caller can confirm the request was proxied to the `OoP` pod.

mod gear;
pub use gear::Hello;

#[doc(hidden)]
pub mod api;
