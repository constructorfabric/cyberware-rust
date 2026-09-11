//! Operator-facing configuration schema for the gear-orchestrator gear.

use std::collections::HashMap;

/// The gear's config section (`gears.gear-orchestrator.config`).
///
/// `deny_unknown_fields` so a misspelled key (e.g. `trusted_registrar`) fails at
/// startup instead of silently deserializing to an empty set. The
/// `internal_auth` migration guard in [`GearOrchestrator::init`] runs first, so
/// it keeps its own targeted error.
///
/// [`GearOrchestrator::init`]: crate::gear::GearOrchestrator
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrchestratorConfig {
    /// Peer identities permitted to act on any gear's registration (see
    /// [`crate::server::make_directory_service`]). Empty by default.
    #[serde(default)]
    pub trusted_registrars: Vec<String>,
    /// Platform-controlled Kubernetes namespaces a `ServiceAccount` peer must
    /// live in to register. Empty disables the namespace check (name-only
    /// fallback).
    #[serde(default)]
    pub platform_namespaces: Vec<String>,
    /// SPIFFE trust domains a workload peer must belong to. Empty disables the
    /// trust-domain check.
    #[serde(default)]
    pub trust_domains: Vec<String>,
    /// Authoritative gRPC-service-name -> owning-gear map. Pins which gear may
    /// advertise a gRPC service name so ownership is decided by this config
    /// rather than by whichever gear self-registers first (which would let a
    /// gear squat a name it was never assigned). A listed name may only be
    /// advertised by the named gear; names absent from the map keep
    /// first-registration ownership. Empty by default.
    #[serde(default)]
    pub grpc_service_owners: HashMap<String, String>,
}
