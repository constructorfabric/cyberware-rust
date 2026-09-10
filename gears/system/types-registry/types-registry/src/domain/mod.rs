//! Domain layer for the Types Registry gear.
//!
//! # Two paths live here at once, and three pairs of names collide
//!
//! P0 builds a database-backed path beside the pre-P0 in-memory one and cuts over
//! at T24–T30, so until then both are present under confusingly close names:
//!
//! | new (keep) | legacy (goes at T24–T30) | what the pair is |
//! |---|---|---|
//! | [`ports`] | [`repo`] | the persistence seam |
//! | [`registry_service`] | [`service`] | the domain surface transports call |
//! | [`enums`] + the row types in [`ports`] | [`model`] | the domain's own vocabulary |
//!
//! # One concept per file until a concept earns a directory
//!
//! [`admission`] is a directory because the operation pipeline has six modules.
//! The grouping axis is the concept — which is also the table in `database.sql` —
//! never "these are all pure functions"; `docs/p0/todo.md` records why a
//! `rules/`-style bucket was rejected.

// ---------------------------------------------------------------------------
// The database-backed path (P0)
// ---------------------------------------------------------------------------

// The synchronous acceptance path and the request identity it rests on (T7).
pub mod admission;
// Materialized effective artifacts and the resolution fingerprint (SPEC D3).
pub mod artifacts;
// The three direct dependency edge kinds, extracted from authored content and the identifier.
pub mod dependency;
// Version-family key derivation and the three family rules.
pub mod family;
// The transient `gts-rust` store, one per admission unit (SPEC D2, §8.2).
pub mod gts_store;
// The registration-policy allowlist (DESIGN §3.2, SPEC §10.3).
pub mod policy;
// The persistence ports, and the rows and inputs that cross them.
pub mod ports;
// The database-backed domain surface every transport adapter calls (SPEC §8.4).
pub mod registry_service;

// ---------------------------------------------------------------------------
// Shared by both paths
// ---------------------------------------------------------------------------

// The domain's own enumeration vocabularies, free of the storage numbering.
pub mod enums;
pub mod error;

// ---------------------------------------------------------------------------
// LEGACY — the pre-P0 in-memory path, deleted at T24-T30
// ---------------------------------------------------------------------------
//
// `repo`/`GtsRepository` (with `InMemoryGtsRepository` and the `switch_to_ready`
// ready-mode split) goes at T24; `service` with it; `model` follows
// `TypesRegistryClient` at T26 (D6) and the cache retyping at T30.
//
// Nothing new should reference these three: a read that needs the database goes
// through `ports`, a transport that needs the domain through `registry_service`.

pub mod model;
pub mod repo;
pub mod service;

// === LOCAL CLIENT ===
// Survives the cutover but is retyped onto `EntitySnapshot` at T30, when the old
// models go.
pub mod local_client;

pub use error::DomainError;
pub use repo::GtsRepository;
pub use service::TypesRegistryService;
