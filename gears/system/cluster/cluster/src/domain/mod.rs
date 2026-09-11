//! The gear's own state and logic — what is left once transport and operator input
//! are taken away.
//!
//! The platform's DDD-light layout (`02_gear_layout_and_sdk_pattern.md:44`) puts this
//! layer between `api/` (transport adapters) and `infra/` (persistence and IO). This
//! gear has no `infra/`: it owns no store, because a backend's persistence belongs to
//! the plugin that implements it — the same reason `nodes-registry` ships without one.
//!
//! # What decides whether a file belongs here: does it exist without the wire?
//!
//! "Domain" is the platform's word (`02:403` puts [`local_client`] here by name, and
//! the resolver gears — `authz-resolver`, `authn-resolver`, `tenant-resolver` — hold
//! `domain/{error,local_client,service}.rs` with no business entities either). It is
//! not a good description of a composition root, so do not reach for the DDD reading
//! when deciding where a new file goes. **Use the wire test instead:**
//!
//! Everything in this module is needed by a Profile 1 process that links no transport
//! at all. Nothing here knows a request arrived. The two pieces of server-side state
//! that fail that test — [`ElectionSubscriptions`](crate::api::grpc::ElectionSubscriptions)
//! and its sweep — live under `api/grpc/` instead, and correctly so: a subscription is
//! an open channel to one client through one replica, so it cannot exist off the wire
//! (§5.4, §6.6). That is the same cut invariant I1 makes between the two profiles.
//!
//! # There is deliberately no `service.rs`
//!
//! The resolver gears put their business logic in `domain/service.rs`. This gear has
//! none to put there: invariant **I14** requires the gRPC services to call the backend
//! `Arc` with no wrapper interposed (`api/grpc/mod.rs:15-17`), so nothing sits between
//! the wire and the backend. The coordination logic lives in the plugins and, for the
//! cache-derived primitives, in [`defaults`](crate::defaults). A `domain::service`
//! here would be a violation, not an omission.
//!
//! - [`registry`] — the per-profile, per-primitive backend table every gRPC service
//!   dispatches through, and the one place an unbound profile becomes
//!   `ProfileNotBound` (§5.2).
//! - [`wiring`] — the builder/handle pair that turns operator config into registered
//!   backends, and the single shutdown entry point. Exposed as embeddable library API
//!   per the DESIGN §3.7 amendment.
//! - [`provider`] — the plugin dispatch table the wiring resolves each binding against.
//! - [`health`] — the readiness contributor that fans out probes across bound profiles.
//! - [`local_client`] — the in-process `ClusterClient`, which makes Profile 1
//!   resolve to a real backend `Arc` with no wrapper on the request path (invariant I1).
//!
//! Every module here is private; `lib.rs` re-exports the public surface flat, so a
//! consumer's path never mentions `domain`.

// `pub` here, but `domain` itself is private in `lib.rs`, so the effective
// visibility is crate-only — the same shape `authz-resolver` and `event-broker` use
// inside their own `domain/`. Spelling these `pub(crate)` instead would be a
// `clippy::redundant_pub_crate` error under the workspace lints.
pub mod health;
pub mod local_client;
pub mod provider;
pub mod registry;
pub mod wiring;
