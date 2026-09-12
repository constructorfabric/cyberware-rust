//! Wire DTOs for the cluster coordination contract (`cluster.v1`).
//!
//! DESIGN.md These are serde + schemars mirrors of the SDK's
//! coordination types, kept separate rather than derived onto the types
//! themselves because `cpt-cf-clst-constraint-no-serde` constrains the
//! coordination *contract* types ([`CacheEntry`](crate::cache::CacheEntry) and
//! siblings) to stay serde-free. The conversions between the two are explicit
//! and live here.
//!
//! # Why this module is unfeatured
//!
//! The transport is optional; the descriptor is not. [`ProfileDescriptor`] is
//! what makes a backend's *synchronous* `consistency()` / `features()` /
//! `provider_name()` answerable at all (§5.5), and it is the return type of
//! [`ClusterClient::descriptor`](crate::client::ClusterClient::descriptor) —
//! which Profile 1 implements in a process where no gRPC client is linked
//! (§3.1). So the descriptor family carries no transport dependency and no
//! feature gate.
//!
//! # Descriptors are per profile, per primitive
//!
//! A profile binds up to three backends, and each declares its own features and
//! its own provider. The provider recorded here is always the **server-side**
//! one (`"postgres"`), never `"remote"`: when a capability requirement fails,
//! the operator has to see which real backend failed it (§5.5).
//!
//! # Three rules shape every request/response type here
//!
//! - **Every request carries `profile`.** The profile is a request parameter, not
//!   a wiring parameter: the cluster gear resolves it to a bound backend on
//!   arrival (§3.1, §5.2).
//! - **Lease-keyed operations carry a [`LeaseToken`], never a handle.** The lease
//!   is a record in the backing store, so any replica can serve any operation
//!   against it (§5.8.1, invariant I7).
//! - **Every method returns a *named* response type**, including the ones whose
//!   backend counterpart returns `()` or `bool`. Two reasons, both checked
//!   against the merged toolchain rather than assumed:
//!   `toolkit-contract-protogen`'s `method_output_type` accepts only
//!   `TypeRef::Named` — a primitive return is an explicit
//!   `PrimitiveMethodReturn` error and a unit return has no mapping at all — and
//!   a bare `()` could never later grow a field without a wire break, which
//!   invariant I12 forbids. `PutResponse` is empty today and stays additive.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::cache::{CacheConsistency, CacheEntry, CacheEvent, CacheFeatures};
use crate::leader::{LeaderElectionFeatures, LeaderStatus};
use crate::lock::LockFeatures;

// ---------------------------------------------------------------------------
// Byte fields
// ---------------------------------------------------------------------------

/// The schemars representation of an opaque byte field.
///
/// **Verified, not stylistic.** Left to its own derive, schemars renders
/// `Vec<u8>` as `{"type": "array", "items": {"type": "integer"}}`, and
/// `toolkit-contract-protogen` faithfully projects that to `repeated int64` — so
/// every byte of every cache value would travel as a separately tagged varint.
/// protogen emits proto3 `bytes` for `{"type": "string", "format": "byte"}`
/// (`lib.rs:1066`), which this produces.
///
/// The serde representation is deliberately left alone. These DTOs' JSON form is
/// used only by tests and diagnostics — protobuf is the authoritative encoding —
/// so paying for a base64 serde shim to match would buy nothing. `format: "byte"`
/// is also exactly what protobuf's own JSON mapping specifies for `bytes`, so the
/// published schema describes the real wire type either way.
fn proto_bytes_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "string",
        "format": "byte",
    })
}

// ---------------------------------------------------------------------------
// Feature / consistency mirrors
// ---------------------------------------------------------------------------

/// Wire mirror of [`CacheConsistency`].
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::profile::WireCacheConsistency")
)]
#[serde(rename_all = "snake_case")]
pub enum WireCacheConsistency {
    /// Linearizable reads and writes.
    Linearizable,
    /// Eventually consistent.
    /// Also the `_UNSPECIFIED = 0` fallback: the weaker guarantee, so an unspecified value fails a `Linearizable` capability
    /// requirement rather than falsely satisfying it.
    #[default]
    EventuallyConsistent,
}

impl From<CacheConsistency> for WireCacheConsistency {
    fn from(value: CacheConsistency) -> Self {
        match value {
            CacheConsistency::Linearizable => Self::Linearizable,
            CacheConsistency::EventuallyConsistent => Self::EventuallyConsistent,
        }
    }
}

impl From<WireCacheConsistency> for CacheConsistency {
    fn from(value: WireCacheConsistency) -> Self {
        match value {
            WireCacheConsistency::Linearizable => Self::Linearizable,
            WireCacheConsistency::EventuallyConsistent => Self::EventuallyConsistent,
        }
    }
}

/// Wire mirror of [`CacheFeatures`].
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::profile::WireCacheFeatures")
)]
// Matches the domain twin `CacheFeatures`: a wire mirror expected to grow fields
// (this one just gained `watch`) must stay `#[non_exhaustive]` so the next
// addition is additive for out-of-crate constructors, not a breaking change.
#[non_exhaustive]
pub struct WireCacheFeatures {
    /// Whether the backend natively supports prefix watches.
    pub prefix_watch: bool,
    /// Whether the backend supports exact-key watches. `None` from a peer that
    /// predates this field (an older server), decoded as `true` — every backend
    /// served exact watch before the field existed, so absence means "supported"
    /// rather than the proto3 default of `false`.
    pub watch: Option<bool>,
}

impl From<CacheFeatures> for WireCacheFeatures {
    fn from(value: CacheFeatures) -> Self {
        Self {
            prefix_watch: value.prefix_watch,
            watch: Some(value.watch),
        }
    }
}

impl From<WireCacheFeatures> for CacheFeatures {
    fn from(value: WireCacheFeatures) -> Self {
        // Absent (`None`) means an old peer that predates the field: it served
        // exact watch, so decode to `true` rather than the proto3 default.
        let watch = value.watch.unwrap_or(true);
        // Enforce the `without_watch()` invariant at the wire trust boundary — a
        // backend that cannot watch one key cannot watch a family of them — so a
        // skewed or hand-crafted peer that pairs `watch: Some(false)` with
        // `prefix_watch: true` is normalized to the honest state rather than
        // admitted as the impossible one the domain constructors forbid. A
        // conforming peer never sends it (`CacheFeatures` is constructor-only),
        // so this only ever narrows a malformed input.
        Self {
            watch,
            prefix_watch: watch && value.prefix_watch,
        }
    }
}

/// Wire mirror of [`LockFeatures`].
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::profile::WireLockFeatures")
)]
pub struct WireLockFeatures {
    /// Whether the backend provides linearizable (correctness-grade) exclusion.
    pub linearizable: bool,
}

impl From<LockFeatures> for WireLockFeatures {
    fn from(value: LockFeatures) -> Self {
        Self {
            linearizable: value.linearizable,
        }
    }
}

impl From<WireLockFeatures> for LockFeatures {
    fn from(value: WireLockFeatures) -> Self {
        Self::new(value.linearizable)
    }
}

/// Wire mirror of [`LeaderElectionFeatures`].
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::profile::WireLeaderElectionFeatures")
)]
pub struct WireLeaderElectionFeatures {
    /// Whether the backend elects a single leader under partition.
    pub linearizable: bool,
}

impl From<LeaderElectionFeatures> for WireLeaderElectionFeatures {
    fn from(value: LeaderElectionFeatures) -> Self {
        Self {
            linearizable: value.linearizable,
        }
    }
}

impl From<WireLeaderElectionFeatures> for LeaderElectionFeatures {
    fn from(value: WireLeaderElectionFeatures) -> Self {
        Self::new(value.linearizable)
    }
}

// ---------------------------------------------------------------------------
// Descriptors
// ---------------------------------------------------------------------------

/// What a profile's cache binding declares — the source of a remote backend's
/// synchronous `consistency()` / `features()` / `provider_name()` answers
/// (§5.5).
#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::profile::CacheDescriptor")
)]
pub struct CacheDescriptor {
    /// The bound backend's consistency class.
    pub consistency: WireCacheConsistency,
    /// The bound backend's native features.
    #[cfg_attr(feature = "grpc-client", proto_bridge(message))]
    pub features: WireCacheFeatures,
    /// The **server-side** provider name (`"postgres"`), never `"remote"`.
    pub provider: String,
}

/// What a profile's distributed-lock binding declares.
#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::profile::LockDescriptor")
)]
pub struct LockDescriptor {
    /// The bound backend's native features.
    #[cfg_attr(feature = "grpc-client", proto_bridge(message))]
    pub features: WireLockFeatures,
    /// The **server-side** provider name.
    pub provider: String,
}

/// What a profile's leader-election binding declares.
#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::profile::LeaderElectionDescriptor")
)]
pub struct LeaderElectionDescriptor {
    /// The bound backend's native features.
    #[cfg_attr(feature = "grpc-client", proto_bridge(message))]
    pub features: WireLeaderElectionFeatures,
    /// The **server-side** provider name.
    pub provider: String,
}

/// Per-profile health, mirroring the per-profile dimension of the composite
/// healthcheck (§4.4).
///
/// It rides the descriptor so a consumer of a degraded profile can leave
/// rotation while consumers of healthy profiles keep serving — a distinction the
/// gear-granular dependency gate cannot express. Clients re-read it on a poll
/// rather than only on a registry-generation change, because health moves
/// without a configuration change, and the poll is also what returns a consumer
/// to rotation once the backend recovers (§5.5).
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::profile::ProfileHealth")
)]
#[serde(rename_all = "snake_case")]
pub enum ProfileHealth {
    /// Every bound backend's `probe()` is passing.
    Serving,
    /// At least one bound backend's `probe()` is failing.
    /// Also the `_UNSPECIFIED = 0` fallback: the fail-safe reading: an unspecified health pulls consumers out of rotation
    /// rather than keeping them in (§4.4).
    #[default]
    Degraded,
}

/// Everything a consumer needs to know about one profile without a call per
/// question — the payload of `DescribeProfiles` (§5.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::profile::ProfileDescriptor")
)]
pub struct ProfileDescriptor {
    /// The profile name.
    pub name: String,
    /// The cache binding.
    #[cfg_attr(feature = "grpc-client", proto_bridge(message))]
    pub cache: CacheDescriptor,
    /// The distributed-lock binding.
    #[cfg_attr(feature = "grpc-client", proto_bridge(message))]
    pub lock: LockDescriptor,
    /// The leader-election binding.
    #[cfg_attr(feature = "grpc-client", proto_bridge(message))]
    pub leader_election: LeaderElectionDescriptor,
    /// The profile's current health.
    pub health: ProfileHealth,
}

// ---------------------------------------------------------------------------
// Shared wire types
// ---------------------------------------------------------------------------

/// The whole authority for a lease-keyed operation (§5.8.1).
///
/// Opaque to the consumer: it is produced by `try_lock` / `lock` / `join`,
/// presented on `renew` / `release` / `resign`, and never inspected above the
/// backend seam. The fields are named rather than a blob so the server can
/// predicate on them directly and an operator can read one out of a log.
///
/// `fence` is deliberately **not** surfaced on
/// [`LockGuard`](crate::lock::LockGuard): it is monotonic only within
/// `fence_retention`, which is enough for cluster's own predicates and not enough
/// to promise a third-party resource (§5.8.1).
///
/// This is the **wire mirror** of [`lease::LeaseToken`](crate::lease::LeaseToken),
/// which is the serde-free form the plugin-facing backend traits name. The pair
/// exists so the projection's derives stop at the `*Api` boundary; converting is a
/// field move in either direction over the three wire fields.
///
/// The SDK-side token additionally carries a replica-local `deadline` (the
/// renewal task's leadership-validity bound, §7.3), which has no wire
/// representation and no place here: a deadline is a liveness annotation the
/// holding process arms for itself, not a shared record field. Converting from
/// this wire form therefore leaves it unarmed (`None`); converting to it drops it.
#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::lock::LeaseToken")
)]
pub struct LeaseToken {
    /// Lock name or election name — the lease's identity within the profile.
    pub name: String,
    /// The acquiring client's rendered `ClientId`. Two clients never share one.
    pub owner: String,
    /// Bumped on every acquisition of this name, including a steal-on-expiry.
    pub fence: u64,
}

impl From<crate::lease::LeaseToken> for LeaseToken {
    fn from(value: crate::lease::LeaseToken) -> Self {
        Self {
            name: value.name,
            owner: value.owner,
            fence: value.fence,
        }
    }
}

impl From<LeaseToken> for crate::lease::LeaseToken {
    fn from(value: LeaseToken) -> Self {
        Self {
            name: value.name,
            owner: value.owner,
            fence: value.fence,
            // The wire carries no deadline; the holder arms it locally on the
            // first successful acquire/renew.
            deadline: None,
        }
    }
}

/// The request shape shared by every lease-keyed operation: `profile` routes it,
/// the token decides it (§5.8.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::lock::LeaseRef")
)]
pub struct LeaseRef {
    /// The profile whose backend serves this lease.
    pub profile: String,
    /// The lease this operation is predicated on.
    #[cfg_attr(feature = "grpc-client", proto_bridge(message))]
    pub token: LeaseToken,
    /// Renewal only — the new lease duration. Absent on release and resign.
    pub ttl_ms: Option<u64>,
    /// Server-side dedup key for the retry that must not happen (§6.10). Most
    /// valuable here and on acquisition, where a lost response is the expensive
    /// case. Unused in phase 1; present so dedup can land without a wire break.
    pub client_request_id: Option<String>,
}

/// A terminal error carried in-band on a watch stream (§6.8's `Closed(err)`).
///
/// It carries the four fields the `Problem` envelope needs to reconstruct a typed
/// variant — domain, code, detail and the `context["data"]` payload — rather than
/// embedding [`Problem`](toolkit_canonical_errors::Problem) itself, which derives
/// serde but **not** `JsonSchema`, and every wire type needs a schemars schema.
/// [`convert`](crate::convert) owns the projection in both directions, so there is
/// still exactly one error codec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::cache::WireError")
)]
pub struct WireError {
    /// The owning namespace — `"cluster.v1"` for everything cluster raises.
    pub error_domain: String,
    /// The machine-readable variant identifier within the domain.
    pub error_code: String,
    /// Human-readable detail. Carried so an unrecognised code from a newer server
    /// still yields a usable message (§6.11's skew rule).
    pub detail: String,
    /// The variant's payload fields as JSON text, exactly as
    /// `#[derive(ContractError)]` writes them into `context["data"]`.
    ///
    /// Text rather than a structured value because protobuf has no untyped-value
    /// field and `toolkit-contract-protogen` rejects a schema with no `type` —
    /// verified, not assumed. [`convert`](crate::convert) parses and renders it,
    /// so the payload is still exactly the derive's own shape.
    pub data: String,
}

/// Wire mirror of [`CacheEntry`].
///
/// `expires_at_ms` has no counterpart on [`CacheEntry`], which carries no expiry:
/// it is populated on the way out when the backend knows it and dropped on the
/// way in. So a `WireCacheEntry` → [`CacheEntry`] → `WireCacheEntry` round trip
/// loses it, while the reverse round trip is lossless.
#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::cache::WireCacheEntry")
)]
pub struct WireCacheEntry {
    /// The stored bytes. Opaque by contract.
    #[schemars(schema_with = "proto_bytes_schema")]
    pub value: Vec<u8>,
    /// The monotonic version (`>= 1`), per-key and only while the key exists.
    pub version: u64,
    /// Absolute expiry in epoch milliseconds, when the backend reports one.
    pub expires_at_ms: Option<u64>,
}

impl From<CacheEntry> for WireCacheEntry {
    fn from(value: CacheEntry) -> Self {
        Self {
            value: value.value,
            version: value.version,
            expires_at_ms: None,
        }
    }
}

impl From<WireCacheEntry> for CacheEntry {
    fn from(value: WireCacheEntry) -> Self {
        Self {
            value: value.value,
            version: value.version,
        }
    }
}

// ---------------------------------------------------------------------------
// Cache contract
// ---------------------------------------------------------------------------

/// `get` — read one key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::cache::GetRequest")
)]
pub struct GetRequest {
    /// The profile whose cache backend serves this read.
    pub profile: String,
    /// The key to read.
    pub key: String,
}

/// `get` — the entry, or its absence. A missing key is `None`, never an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::cache::GetResponse")
)]
pub struct GetResponse {
    /// The entry, or `None` when the key is absent.
    #[cfg_attr(feature = "grpc-client", proto_bridge(message))]
    pub entry: Option<WireCacheEntry>,
}

/// `put` and `put_if_absent` — the shared write shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::cache::PutRequest")
)]
pub struct PutRequest {
    /// The profile whose cache backend serves this write.
    pub profile: String,
    /// The key to write.
    pub key: String,
    /// The value bytes. Opaque by contract.
    #[schemars(schema_with = "proto_bytes_schema")]
    pub value: Vec<u8>,
    /// Time-to-live in milliseconds; `None` stores the entry indefinitely.
    pub ttl_ms: Option<u64>,
    /// See [`LeaseRef::client_request_id`]. Unused in phase 1.
    pub client_request_id: Option<String>,
}

/// `put` — acknowledgement.
///
/// The backend's `put` returns `()`, so there is nothing operation-specific to
/// report; the response carries only the registry generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::cache::PutResponse")
)]
pub struct PutResponse {
    /// The `ProfileRegistry` generation that served this operation (§5.2).
    ///
    /// Present for two reasons. It is §5.6's staleness detector, so a client
    /// learns the server's profile registry moved without waiting for its
    /// descriptor poll — free here, since the response has to carry something.
    /// And it has to carry something: `toolkit-contract-protogen` rejects a
    /// message with no fields (`object without properties`), verified by running
    /// it, so neither the design's `Result<(), _>` nor an empty named response is
    /// projectable.
    pub generation: u64,
}

/// `put_if_absent` — the created entry, or `None` when the key already existed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::cache::PutIfAbsentResponse")
)]
pub struct PutIfAbsentResponse {
    /// The entry this call created, or `None` when the key was already present.
    #[cfg_attr(feature = "grpc-client", proto_bridge(message))]
    pub created: Option<WireCacheEntry>,
}

/// `compare_and_swap` — a version-guarded write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::cache::CasRequest")
)]
pub struct CasRequest {
    /// The profile whose cache backend serves this write.
    pub profile: String,
    /// The key to swap.
    pub key: String,
    /// The version the caller believes is current.
    pub expected_version: u64,
    /// The value bytes to store.
    #[schemars(schema_with = "proto_bytes_schema")]
    pub new_value: Vec<u8>,
    /// Time-to-live in milliseconds; `None` stores the entry indefinitely.
    pub ttl_ms: Option<u64>,
}

/// `compare_and_swap` — the entry as written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::cache::CasResponse")
)]
pub struct CasResponse {
    /// The stored entry, carrying its new version.
    #[cfg_attr(feature = "grpc-client", proto_bridge(message))]
    pub entry: WireCacheEntry,
}

/// `compare_and_delete` — a value-guarded delete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::cache::CadRequest")
)]
pub struct CadRequest {
    /// The profile whose cache backend serves this delete.
    pub profile: String,
    /// The key to delete.
    pub key: String,
    /// The value the caller believes is stored; a mismatch is a no-op.
    #[schemars(schema_with = "proto_bytes_schema")]
    pub expected_value: Vec<u8>,
}

/// `compare_and_delete` — whether the guarded delete matched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::cache::CadResponse")
)]
pub struct CadResponse {
    /// `true` when the value matched and the key was removed.
    pub deleted: bool,
}

/// `delete` — remove one key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::cache::DeleteRequest")
)]
pub struct DeleteRequest {
    /// The profile whose cache backend serves this delete.
    pub profile: String,
    /// The key to remove.
    pub key: String,
}

/// `delete` — whether the key existed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::cache::DeleteResponse")
)]
pub struct DeleteResponse {
    /// Whether the key was present. Best-effort `true` when the backend cannot
    /// determine prior existence, matching the backend trait.
    pub existed: bool,
}

/// `contains` — existence check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::cache::ContainsRequest")
)]
pub struct ContainsRequest {
    /// The profile whose cache backend serves this read.
    pub profile: String,
    /// The key to test.
    pub key: String,
}

/// `contains` — the answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::cache::ContainsResponse")
)]
pub struct ContainsResponse {
    /// Whether the key is present.
    pub present: bool,
}

/// `scan_prefix` — one page of keys under a prefix.
///
/// Paginated on the wire even though the backend trait returns a whole
/// `Vec<String>`: the client reassembles it by looping pages (§6.4), so an
/// unbounded keyspace cannot produce an unbounded single message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::cache::ScanRequest")
)]
pub struct ScanRequest {
    /// The profile whose cache backend serves this scan.
    pub profile: String,
    /// The key prefix to enumerate.
    pub prefix: String,
    /// Maximum keys to return; the server may return fewer.
    ///
    /// `u64` rather than `u32` because protogen's primitive map has no `uint32`
    /// arm — a `u32` projects to `int64`, which is both wider and signed.
    pub page_size: Option<u64>,
    /// The `next_page_token` from the previous page; absent for the first page.
    pub page_token: Option<String>,
}

/// `scan_prefix` — one page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::cache::ScanResponse")
)]
pub struct ScanResponse {
    /// The keys in this page.
    pub keys: Vec<String>,
    /// Token for the next page, or `None` when this page is the last.
    pub next_page_token: Option<String>,
}

/// `watch` — subscribe to one exact key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::cache::WatchRequest")
)]
pub struct WatchRequest {
    /// The profile whose cache backend serves this watch.
    pub profile: String,
    /// The key to watch.
    pub key: String,
}

/// `watch_prefix` — subscribe to a key prefix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::cache::WatchPrefixRequest")
)]
pub struct WatchPrefixRequest {
    /// The profile whose cache backend serves this watch.
    pub profile: String,
    /// The key prefix to watch.
    pub prefix: String,
}

/// Which kind of watch event this is — the discriminator of the flat wire event.
///
/// A unit-only enum, which projects to a proto3 enum. See
/// [`WireCacheWatchEvent`] for why the wire event is flat rather than a `oneof`.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::cache::CacheWatchEventKind")
)]
#[serde(rename_all = "snake_case")]
pub enum CacheWatchEventKind {
    /// The key was created or updated.
    Changed,
    /// The key was deleted.
    Deleted,
    /// The key's TTL elapsed and it was removed.
    Expired,
    /// The watcher fell behind; `dropped` events were lost.
    Lagged,
    /// The subscription was re-established; re-read.
    /// Also the `_UNSPECIFIED = 0` fallback: "you may have missed something, re-read" — exactly the semantics of an
    /// unrecognised event (§6.8).
    #[default]
    Reset,
    /// Terminal. The server sends this, then closes the stream.
    Closed,
}

/// Wire mirror of [`CacheWatchEvent`](crate::cache::CacheWatchEvent) — a **flat
/// discriminated message**, not a `oneof`.
///
/// The shape is dictated by `toolkit-contract-protogen`, verified by running it
/// rather than inferred. Two of its rules rule out the obvious encodings:
///
/// - an externally-tagged enum projects to a `oneof` only when every branch's
///   single property is a `$ref` to a **named** message. A struct variant yields
///   an inline object and is rejected (`unknown primitive type "object"`);
/// - a message with **no fields is rejected outright** (`object without
///   properties`). So the payload-free variants — `Reset`, and `Lagged` before it
///   gained a count — have no message to `$ref`, and the `oneof` encoding cannot
///   represent them at all.
///
/// So the wire carries a discriminator plus the union of the payloads, each
/// optional and populated only for the kinds that have it. The invariant "the kind
/// decides which fields are present" is enforced at the seam rather than by the
/// wire type: [`convert`](crate::convert) reconstructs
/// [`CacheWatchEvent`](crate::cache::CacheWatchEvent), which *is* a proper Rust
/// union, so nothing above the §3.1 seam ever sees this shape (§6.8).
///
/// It is also the more additive shape (invariant I12): a new event kind is a new
/// enum value plus, if it needs one, a new optional field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::cache::WireCacheWatchEvent")
)]
pub struct WireCacheWatchEvent {
    /// Which kind of event this is. Decides which fields below are populated.
    pub kind: CacheWatchEventKind,
    /// The affected key — `Changed`, `Deleted` and `Expired` only.
    pub key: Option<String>,
    /// The number of events dropped — `Lagged` only.
    pub dropped: Option<u64>,
    /// The terminal error — `Closed` only.
    #[cfg_attr(feature = "grpc-client", proto_bridge(message))]
    pub error: Option<WireError>,
}

impl WireCacheWatchEvent {
    /// A key-mutation event.
    #[must_use]
    pub fn key_event(kind: CacheWatchEventKind, key: String) -> Self {
        Self {
            kind,
            key: Some(key),
            dropped: None,
            error: None,
        }
    }

    /// A `Lagged` event.
    #[must_use]
    pub fn lagged(dropped: u64) -> Self {
        Self {
            kind: CacheWatchEventKind::Lagged,
            key: None,
            dropped: Some(dropped),
            error: None,
        }
    }

    /// A `Reset` event, which carries nothing beyond having happened.
    #[must_use]
    pub fn reset() -> Self {
        Self {
            kind: CacheWatchEventKind::Reset,
            key: None,
            dropped: None,
            error: None,
        }
    }

    /// A terminal `Closed` event.
    #[must_use]
    pub fn closed(error: WireError) -> Self {
        Self {
            kind: CacheWatchEventKind::Closed,
            key: None,
            dropped: None,
            error: Some(error),
        }
    }
}

impl From<CacheEvent> for WireCacheWatchEvent {
    fn from(value: CacheEvent) -> Self {
        match value {
            CacheEvent::Changed { key } => Self::key_event(CacheWatchEventKind::Changed, key),
            CacheEvent::Deleted { key } => Self::key_event(CacheWatchEventKind::Deleted, key),
            CacheEvent::Expired { key } => Self::key_event(CacheWatchEventKind::Expired, key),
        }
    }
}

// ---------------------------------------------------------------------------
// Distributed-lock contract
// ---------------------------------------------------------------------------

/// `try_lock` — insert-or-steal-if-expired. The one lease operation that carries
/// no token, because it mints one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::lock::TryLockRequest")
)]
pub struct TryLockRequest {
    /// The profile whose lock backend serves this acquisition.
    pub profile: String,
    /// The lock name.
    pub name: String,
    /// The lease duration in milliseconds.
    pub ttl_ms: u64,
    /// Server-side dedup for the retry that must not happen (§6.10): a lost
    /// response on an acquire is the expensive case, because a blind retry
    /// reports `LockContended` against the caller's *own* lease.
    pub client_request_id: Option<String>,
}

/// `lock` — `try_lock` plus server-side waiting, bounded by `timeout_ms`.
///
/// Named `LockRequest` on the wire per §12.1. It is deliberately **not**
/// re-exported at the crate root, where it would collide with the SDK's existing
/// [`LockRequest`](crate::lock::LockRequest) guard-command enum — these DTOs are
/// types no consumer names (§6.2), so `dto::` is the only path to them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::lock::LockRequest")
)]
pub struct LockRequest {
    /// The profile whose lock backend serves this acquisition.
    pub profile: String,
    /// The lock name.
    pub name: String,
    /// The lease duration in milliseconds.
    pub ttl_ms: u64,
    /// How long the server waits before giving up, in milliseconds.
    pub timeout_ms: u64,
    /// See [`TryLockRequest::client_request_id`].
    pub client_request_id: Option<String>,
}

/// `try_lock` / `lock` — the minted lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::lock::LockAcquired")
)]
pub struct LockAcquired {
    /// The authority for every subsequent operation on this lease.
    #[cfg_attr(feature = "grpc-client", proto_bridge(message))]
    pub token: LeaseToken,
}

/// `renew` — acknowledgement.
///
/// A renewal that matches nothing is an **error**, not an empty success: the caller
/// must learn it lost the lease (§6.10). So this type only ever describes the
/// successful case, and has nothing operation-specific to add to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::lock::RenewResponse")
)]
pub struct RenewResponse {
    /// The `ProfileRegistry` generation that served this operation (§5.2).
    ///
    /// Present for two reasons. It is §5.6's staleness detector, so a client
    /// learns the server's profile registry moved without waiting for its
    /// descriptor poll — free here, since the response has to carry something.
    /// And it has to carry something: `toolkit-contract-protogen` rejects a
    /// message with no fields (`object without properties`), verified by running
    /// it, so neither the design's `Result<(), _>` nor an empty named response is
    /// projectable.
    pub generation: u64,
}

/// `release` — acknowledgement, and its emptiness is load-bearing.
///
/// Release is idempotent by absence (§6.10): a token matching nothing has already
/// achieved what the caller wanted. Reporting *whether* a record matched would let
/// a caller use `release` to probe whether a token was ever valid, which §5.8.1
/// forbids — both answers must be indistinguishable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::lock::ReleaseResponse")
)]
pub struct ReleaseResponse {
    /// The `ProfileRegistry` generation that served this operation (§5.2).
    ///
    /// Present for two reasons. It is §5.6's staleness detector, so a client
    /// learns the server's profile registry moved without waiting for its
    /// descriptor poll — free here, since the response has to carry something.
    /// And it has to carry something: `toolkit-contract-protogen` rejects a
    /// message with no fields (`object without properties`), verified by running
    /// it, so neither the design's `Result<(), _>` nor an empty named response is
    /// projectable.
    pub generation: u64,
}

// ---------------------------------------------------------------------------
// Leader-election contract
// ---------------------------------------------------------------------------

/// Wire mirror of [`LeaderStatus`].
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::leader::WireLeaderStatus")
)]
#[serde(rename_all = "snake_case")]
pub enum WireLeaderStatus {
    /// This participant currently holds the claim.
    Leader,
    /// Another participant holds the claim.
    /// Also the `_UNSPECIFIED = 0` fallback: never claim leadership from a value this build does not understand; the next
    /// real `Status` event resolves it.
    #[default]
    Follower,
    /// The claim was lost. Transient — the watch auto-reenrolls.
    Lost,
}

impl From<LeaderStatus> for WireLeaderStatus {
    fn from(value: LeaderStatus) -> Self {
        match value {
            LeaderStatus::Leader => Self::Leader,
            LeaderStatus::Follower => Self::Follower,
            LeaderStatus::Lost => Self::Lost,
        }
    }
}

impl From<WireLeaderStatus> for LeaderStatus {
    fn from(value: WireLeaderStatus) -> Self {
        match value {
            WireLeaderStatus::Leader => Self::Leader,
            WireLeaderStatus::Follower => Self::Follower,
            WireLeaderStatus::Lost => Self::Lost,
        }
    }
}

/// `join` — enrol in a named election. Mints a lease, exactly as `try_lock` does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::leader::JoinRequest")
)]
pub struct JoinRequest {
    /// The profile whose leader-election backend serves this election.
    pub profile: String,
    /// The election name.
    pub name: String,
    /// The claim's lease duration in milliseconds.
    pub ttl_ms: u64,
    /// How many consecutive renewal failures are tolerated before the claim is
    /// reported lost. Mirrors
    /// [`ElectionConfig::max_missed_renewals`](crate::leader::ElectionConfig::max_missed_renewals)
    /// (a `u8` there; widened here for the same protogen reason as
    /// [`ScanRequest::page_size`]).
    pub max_missed_renewals: Option<u64>,
    /// See [`TryLockRequest::client_request_id`] — a retried `join` reports
    /// another leader when the caller *is* the leader (§6.10).
    pub client_request_id: Option<String>,
}

/// `join` — the minted lease, the subscription id, and the initial status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::leader::LeaderJoined")
)]
pub struct LeaderJoined {
    /// The authority for `renew` and `resign` on this claim.
    #[cfg_attr(feature = "grpc-client", proto_bridge(message))]
    pub token: LeaseToken,
    /// Addresses the *subscription*, not the lease — the one piece of
    /// replica-local state, so `await_change` can report the
    /// subscription's replica going away while the lease is untouched (§5.8.1).
    pub election_id: String,
    /// Whether this participant won the claim on joining.
    pub initial_status: WireLeaderStatus,
}

/// `resign` — acknowledgement, empty for the same reason as [`ReleaseResponse`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::leader::ResignResponse")
)]
pub struct ResignResponse {
    /// The `ProfileRegistry` generation that served this operation (§5.2).
    ///
    /// Present for two reasons. It is §5.6's staleness detector, so a client
    /// learns the server's profile registry moved without waiting for its
    /// descriptor poll — free here, since the response has to carry something.
    /// And it has to carry something: `toolkit-contract-protogen` rejects a
    /// message with no fields (`object without properties`), verified by running
    /// it, so neither the design's `Result<(), _>` nor an empty named response is
    /// projectable.
    pub generation: u64,
}

/// `await_change` — follow one election's transitions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::leader::AwaitChangeRequest")
)]
pub struct AwaitChangeRequest {
    /// The profile whose leader-election backend serves this election.
    pub profile: String,
    /// The subscription minted by `join`.
    pub election_id: String,
}

/// Which kind of leader-watch event this is. Unit-only, so it projects to a
/// proto3 enum — see [`WireCacheWatchEvent`] for the shape rationale.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::leader::LeaderWatchEventKind")
)]
#[serde(rename_all = "snake_case")]
pub enum LeaderWatchEventKind {
    /// A leadership transition. `Lost` is transient (§6.6).
    Status,
    /// The watcher fell behind.
    Lagged,
    /// The subscription was re-established.
    /// Also the `_UNSPECIFIED = 0` fallback: as for the cache union.
    #[default]
    Reset,
    /// Terminal. On graceful shutdown the server sends `Status(Lost)` first, then
    /// this (§4.8).
    Closed,
}

/// Wire mirror of [`LeaderWatchEvent`](crate::leader::LeaderWatchEvent), flat for
/// the same verified protogen reasons as [`WireCacheWatchEvent`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::leader::WireLeaderWatchEvent")
)]
pub struct WireLeaderWatchEvent {
    /// Which kind of event this is. Decides which fields below are populated.
    pub kind: LeaderWatchEventKind,
    /// The new status — `Status` only.
    #[cfg_attr(feature = "grpc-client", proto_bridge(message))]
    pub status: Option<WireLeaderStatus>,
    /// The number of events dropped — `Lagged` only.
    pub dropped: Option<u64>,
    /// The terminal error — `Closed` only.
    #[cfg_attr(feature = "grpc-client", proto_bridge(message))]
    pub error: Option<WireError>,
}

impl WireLeaderWatchEvent {
    /// A leadership transition.
    #[must_use]
    pub fn status(status: WireLeaderStatus) -> Self {
        Self {
            kind: LeaderWatchEventKind::Status,
            status: Some(status),
            dropped: None,
            error: None,
        }
    }

    /// A `Lagged` event.
    #[must_use]
    pub fn lagged(dropped: u64) -> Self {
        Self {
            kind: LeaderWatchEventKind::Lagged,
            status: None,
            dropped: Some(dropped),
            error: None,
        }
    }

    /// A `Reset` event.
    #[must_use]
    pub fn reset() -> Self {
        Self {
            kind: LeaderWatchEventKind::Reset,
            status: None,
            dropped: None,
            error: None,
        }
    }

    /// A terminal `Closed` event.
    #[must_use]
    pub fn closed(error: WireError) -> Self {
        Self {
            kind: LeaderWatchEventKind::Closed,
            status: None,
            dropped: None,
            error: Some(error),
        }
    }
}

// ---------------------------------------------------------------------------
// Profile contract
// ---------------------------------------------------------------------------

/// `describe_profiles` — the whole inventory, or a named subset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::profile::DescribeProfilesRequest")
)]
pub struct DescribeProfilesRequest {
    /// Restrict the response to these profiles; empty means "all".
    pub profiles: Vec<String>,
}

/// `describe_profiles` — the inventory plus the generation it was read from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "grpc-client", derive(toolkit::ProtoBridge))]
#[cfg_attr(
    feature = "grpc-client",
    proto_bridge(stub = "crate::grpc::stubs::profile::DescribeProfilesResponse")
)]
pub struct DescribeProfilesResponse {
    /// One descriptor per requested, bound profile.
    #[cfg_attr(feature = "grpc-client", proto_bridge(message))]
    pub profiles: Vec<ProfileDescriptor>,
    /// The `ProfileRegistry` snapshot generation these descriptors came from
    /// (§5.2). A client re-reads descriptors when it observes a change (§5.6).
    pub generation: u64,
}

#[cfg(test)]
#[path = "dto_tests.rs"]
mod dto_tests;
