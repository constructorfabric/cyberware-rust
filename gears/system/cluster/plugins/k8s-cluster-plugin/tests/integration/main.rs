//! Layer 2/3 integration suite — a **single** test binary so all scenarios
//! share **one** k3s API server.
//!
//! k3s takes ~15-25s to become ready and (unlike a Redis fixture) runs a full
//! control plane in a privileged container, so booting one per scenario — or
//! even one per suite file — is the dominant cost. The fixture keeps the cluster
//! in a `static` [`tokio::sync::OnceCell`] (see `common`), which shares it
//! across every test *in this process*; collapsing the formerly seven separate
//! test binaries into this one is what lets that static serve the whole suite
//! from a single container. Each scenario is still isolated by its own
//! namespace, so the sharing is invisible to the tests.
//!
//! Because there is now one binary, the Makefile/CI no longer loops per-binary
//! with a k3s cleanup between each; a single `--test-threads` cap bounds how many
//! scenarios hit the shared cluster at once (see `test-cluster-k8s`).
#![cfg(feature = "integration")]

mod common;

mod cache_integration;
mod conformance;
mod k8s_specific;
mod leader_integration;
mod lifecycle_integration;
mod lock_integration;
mod watch_integration;
