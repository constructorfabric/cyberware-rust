//! Gear Manager - tracks and manages all live gear instances in the runtime

use dashmap::DashMap;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Represents an endpoint where a gear instance can be reached
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Endpoint {
    pub uri: String,
}

/// Typed view of an endpoint for parsing and matching
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EndpointKind {
    /// TCP endpoint with resolved socket address
    Tcp(std::net::SocketAddr),
    /// Unix domain socket with file path
    Uds(std::path::PathBuf),
    /// Other/unparsed endpoint URI
    Other(String),
}

impl Endpoint {
    pub fn from_uri<S: Into<String>>(s: S) -> Self {
        Self { uri: s.into() }
    }

    pub fn uds(path: impl AsRef<std::path::Path>) -> Self {
        Self {
            uri: format!("unix://{}", path.as_ref().display()),
        }
    }

    #[must_use]
    pub fn http(host: &str, port: u16) -> Self {
        Self {
            uri: format!("http://{host}:{port}"),
        }
    }

    #[must_use]
    pub fn https(host: &str, port: u16) -> Self {
        Self {
            uri: format!("https://{host}:{port}"),
        }
    }

    /// Parse the endpoint URI into a typed view
    #[must_use]
    pub fn kind(&self) -> EndpointKind {
        if let Some(rest) = self.uri.strip_prefix("unix://") {
            return EndpointKind::Uds(std::path::PathBuf::from(rest));
        }
        if let Some(rest) = self.uri.strip_prefix("http://")
            && let Ok(addr) = rest.parse::<std::net::SocketAddr>()
        {
            return EndpointKind::Tcp(addr);
        }
        if let Some(rest) = self.uri.strip_prefix("https://")
            && let Ok(addr) = rest.parse::<std::net::SocketAddr>()
        {
            return EndpointKind::Tcp(addr);
        }
        EndpointKind::Other(self.uri.clone())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstanceState {
    Registered,
    Ready,
    Healthy,
    Quarantined,
    Draining,
}

/// Runtime state of an instance (guarded by `RwLock` for safe mutation)
#[derive(Clone, Debug)]
pub struct InstanceRuntimeState {
    pub last_heartbeat: Instant,
    pub state: InstanceState,
}

/// Represents a single instance of a gear
#[derive(Debug)]
#[must_use]
pub struct GearInstance {
    pub gear: String,
    pub instance_id: Uuid,
    pub control: Option<Endpoint>,
    pub grpc_services: HashMap<String, Endpoint>,
    pub version: Option<String>,
    pub rest_endpoint: Option<Endpoint>,
    pub openapi_spec: Option<String>,
    /// Stable addressing labels (k8s `matchLabels` style) advertised by this
    /// instance, used for label-based instance selection.
    pub labels: BTreeMap<String, String>,
    inner: Arc<parking_lot::RwLock<InstanceRuntimeState>>,
}

impl Clone for GearInstance {
    fn clone(&self) -> Self {
        Self {
            gear: self.gear.clone(),
            instance_id: self.instance_id,
            control: self.control.clone(),
            grpc_services: self.grpc_services.clone(),
            version: self.version.clone(),
            rest_endpoint: self.rest_endpoint.clone(),
            openapi_spec: self.openapi_spec.clone(),
            labels: self.labels.clone(),
            inner: Arc::clone(&self.inner),
        }
    }
}

impl GearInstance {
    /// Build a new instance record using `other`'s metadata but preserving
    /// `self`'s `inner` runtime-state lock. This makes re-registration atomic:
    /// concurrent `update_heartbeat` calls continue to write to the same state
    /// object instead of racing with a copied/swapped `InstanceRuntimeState`.
    fn with_metadata_of(&self, other: &GearInstance) -> GearInstance {
        GearInstance {
            gear: other.gear.clone(),
            instance_id: other.instance_id,
            control: other.control.clone(),
            grpc_services: other.grpc_services.clone(),
            version: other.version.clone(),
            rest_endpoint: other.rest_endpoint.clone(),
            openapi_spec: other.openapi_spec.clone(),
            // Empty incoming labels mean "keep the stored set", not "clear"; a
            // non-empty set replaces it. See `RegisterInstanceInfo::with_labels`
            // for why (self-heal / REST-augmentation re-registers omit labels).
            labels: if other.labels.is_empty() {
                self.labels.clone()
            } else {
                other.labels.clone()
            },
            inner: Arc::clone(&self.inner),
        }
    }
}

impl GearInstance {
    pub fn new(gear: impl Into<String>, instance_id: Uuid) -> Self {
        Self {
            gear: gear.into(),
            instance_id,
            control: None,
            grpc_services: HashMap::new(),
            version: None,
            rest_endpoint: None,
            openapi_spec: None,
            labels: BTreeMap::new(),
            inner: Arc::new(parking_lot::RwLock::new(InstanceRuntimeState {
                last_heartbeat: Instant::now(),
                state: InstanceState::Registered,
            })),
        }
    }

    pub fn with_control(mut self, ep: Endpoint) -> Self {
        self.control = Some(ep);
        self
    }

    pub fn with_version(mut self, v: impl Into<String>) -> Self {
        self.version = Some(v.into());
        self
    }

    pub fn with_grpc_service(mut self, name: impl Into<String>, ep: Endpoint) -> Self {
        self.grpc_services.insert(name.into(), ep);
        self
    }

    pub fn with_rest_endpoint(mut self, ep: Endpoint) -> Self {
        self.rest_endpoint = Some(ep);
        self
    }

    pub fn with_openapi_spec(mut self, spec: impl Into<String>) -> Self {
        self.openapi_spec = Some(spec.into());
        self
    }

    pub fn with_labels(mut self, labels: BTreeMap<String, String>) -> Self {
        self.labels = labels;
        self
    }

    /// Get the current state of this instance
    #[must_use]
    pub fn state(&self) -> InstanceState {
        self.inner.read().state
    }

    /// Get the last heartbeat timestamp
    #[must_use]
    pub fn last_heartbeat(&self) -> Instant {
        self.inner.read().last_heartbeat
    }
}

/// Central registry that tracks all running gear instances in the system.
/// Provides discovery, health tracking, and round-robin load balancing.
///
/// Always shared as an `Arc<GearManager>` and NOT `Clone`: a by-value clone
/// would deep-copy the `DashMap` directories while sharing the guards, so the
/// registration lock would no longer serialize the map it guards.
#[must_use]
pub struct GearManager {
    inner: DashMap<String, Vec<Arc<GearInstance>>>,
    rr_counters: DashMap<String, usize>,
    hb_ttl: Duration,
    hb_grace: Duration,
    /// Serializes gRPC-service-name ownership check + insert in
    /// [`register_instance`](Self::register_instance) so two
    /// gears cannot both pass an "unowned" check and then both commit the same
    /// name. `DashMap` locks per entry, giving no boundary across the
    /// cross-gear ownership scan and the single-entry write; this gate provides
    /// it.
    reg_lock: parking_lot::Mutex<()>,
    /// Authoritative gRPC-service-name -> owning-gear map. When a name appears
    /// here, ownership is fixed to the named gear: only that gear may advertise
    /// it and no other gear can ever claim it, regardless of which registered
    /// first — the stable ownership source the dynamic registration scan is
    /// not. Populated by the runtime at startup (see
    /// [`set_grpc_service_owners`](Self::set_grpc_service_owners)); names absent
    /// from the map fall back to first-registration ownership.
    service_owners: parking_lot::RwLock<HashMap<String, String>>,
}

/// Returned by [`GearManager::register_instance`] when an instance
/// advertises a gRPC service name already owned by a *different* gear. The
/// store is left unchanged; the caller maps this onto a permission-denied
/// response (see `DirectoryServiceNameConflict`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrpcServiceNameConflict {
    /// The gRPC service name that is already owned.
    pub service_name: String,
    /// The gear that currently owns `service_name`.
    pub owner: String,
}

impl std::fmt::Display for GrpcServiceNameConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "gRPC service name '{}' is already owned by gear '{}'",
            self.service_name, self.owner
        )
    }
}

impl std::error::Error for GrpcServiceNameConflict {}

impl std::fmt::Debug for GearManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let gears: Vec<String> = self.inner.iter().map(|e| e.key().clone()).collect();
        f.debug_struct("GearManager")
            .field("instances_count", &self.inner.len())
            .field("gears", &gears)
            .field("heartbeat_ttl", &self.hb_ttl)
            .field("heartbeat_grace", &self.hb_grace)
            .finish_non_exhaustive()
    }
}

impl GearManager {
    pub fn new() -> Self {
        Self {
            inner: DashMap::new(),
            rr_counters: DashMap::new(),
            hb_ttl: Duration::from_secs(15),
            hb_grace: Duration::from_secs(30),
            reg_lock: parking_lot::Mutex::new(()),
            service_owners: parking_lot::RwLock::new(HashMap::new()),
        }
    }

    pub fn with_heartbeat_policy(mut self, ttl: Duration, grace: Duration) -> Self {
        self.hb_ttl = ttl;
        self.hb_grace = grace;
        self
    }

    /// Install the authoritative gRPC-service-name -> owning-gear map.
    ///
    /// Ownership of a listed name is decided by this map, not by registration
    /// order: [`register_instance`](Self::register_instance) rejects any gear
    /// other than the declared owner and always admits the declared owner. This
    /// closes the "squatter" hole where a gear authorized only for its own name
    /// could claim another gear's service simply by registering first. Names
    /// not present keep first-registration ownership.
    ///
    /// The map is sourced from operator config (see the `gear-orchestrator`
    /// gear, which installs it alongside its registration policy). Intended to
    /// be called once at startup, before any gear self-registers; a later call
    /// replaces the map.
    pub fn set_grpc_service_owners(&self, owners: HashMap<String, String>) {
        *self.service_owners.write() = owners;
    }

    /// Register or update a gear instance, enforcing single-gear ownership of
    /// every gRPC service name it advertises — atomically.
    ///
    /// Ownership is resolved in two tiers:
    ///
    /// 1. **Declared (authoritative).** If the name appears in the configured
    ///    ownership map (see
    ///    [`set_grpc_service_owners`](Self::set_grpc_service_owners)), only the
    ///    declared owner may advertise it — any other gear is rejected even if
    ///    it registers first. This stops a gear from squatting a name it was
    ///    never assigned.
    /// 2. **First-registration (fallback).** For names absent from that map, the
    ///    first gear to register the name owns it; a later, *different* gear is
    ///    rejected.
    ///
    /// The ownership scan and the insert run under one registration lock, so two
    /// gears cannot both pass an "unowned" check and then both commit the same
    /// name (a check-then-write race the per-entry `DashMap` locks do not
    /// prevent). A name already owned by the *same* gear is fine (re-register /
    /// additional instance); only a *different* owner is rejected, and on
    /// rejection the store is left untouched.
    ///
    /// Re-registering an existing `instance_id` is an idempotent endpoint /
    /// metadata refresh, NOT a liveness reset: the existing runtime state
    /// (`last_heartbeat` + [`InstanceState`]) is carried over onto the new
    /// record by preserving the same `Arc<RwLock<InstanceRuntimeState>>`.
    /// Without this, a periodic self-heal re-registration (see
    /// `oop_registration::presence_loop`) would knock a `Healthy` instance back
    /// to `Registered`, dropping it out of gRPC round-robin until the next
    /// heartbeat. It would also race with concurrent `update_heartbeat` calls.
    ///
    /// # Errors
    /// Returns [`GrpcServiceNameConflict`] if an advertised gRPC service name is
    /// already owned by another gear.
    pub fn register_instance(
        &self,
        instance: Arc<GearInstance>,
    ) -> Result<(), GrpcServiceNameConflict> {
        let _gate = self.reg_lock.lock();
        let declared = self.service_owners.read();

        // Tier 1: names the config map declares are resolved here; collect the
        // rest for the tier-2 scan below.
        let mut undeclared: Vec<&str> = Vec::new();
        for service_name in instance.grpc_services.keys() {
            match declared.get(service_name) {
                Some(owner) if *owner != instance.gear => {
                    return Err(GrpcServiceNameConflict {
                        service_name: service_name.clone(),
                        owner: owner.clone(),
                    });
                }
                Some(_) => {} // declared owner matches → authorized
                None => undeclared.push(service_name),
            }
        }
        drop(declared);

        // Tier 2: one pass over the directory, checking every undeclared name
        // per instance visited rather than re-scanning once per name. A name
        // already held by a *different* gear conflicts.
        if !undeclared.is_empty() {
            for entry in &self.inner {
                if entry.key() == &instance.gear {
                    continue; // same gear: re-register / additional instance is fine
                }
                for existing in entry.value() {
                    if let Some(&name) = undeclared
                        .iter()
                        .find(|&&n| existing.grpc_services.contains_key(n))
                    {
                        return Err(GrpcServiceNameConflict {
                            service_name: name.to_owned(),
                            owner: entry.key().clone(),
                        });
                    }
                }
            }
        }

        let gear = instance.gear.clone();
        let mut vec = self.inner.entry(gear).or_default();
        // replace by instance_id if it already exists
        if let Some(pos) = vec
            .iter()
            .position(|i| i.instance_id == instance.instance_id)
        {
            vec[pos] = Arc::new(vec[pos].with_metadata_of(&instance));
        } else {
            vec.push(instance);
        }
        Ok(())
    }

    /// Mark an instance as ready
    pub fn mark_ready(&self, gear: &str, instance_id: Uuid) {
        if let Some(mut vec) = self.inner.get_mut(gear)
            && let Some(inst) = vec.iter_mut().find(|i| i.instance_id == instance_id)
        {
            let mut state = inst.inner.write();
            state.state = InstanceState::Ready;
        }
    }

    /// Update the heartbeat timestamp for an instance
    pub fn update_heartbeat(&self, gear: &str, instance_id: Uuid, at: Instant) {
        if let Some(mut vec) = self.inner.get_mut(gear)
            && let Some(inst) = vec.iter_mut().find(|i| i.instance_id == instance_id)
        {
            let mut state = inst.inner.write();
            state.last_heartbeat = at;
            // Transition Registered -> Healthy on first heartbeat
            if state.state == InstanceState::Registered {
                state.state = InstanceState::Healthy;
            }
        }
    }

    /// Mark an instance as quarantined
    pub fn mark_quarantined(&self, gear: &str, instance_id: Uuid) {
        if let Some(mut vec) = self.inner.get_mut(gear)
            && let Some(inst) = vec.iter_mut().find(|i| i.instance_id == instance_id)
        {
            inst.inner.write().state = InstanceState::Quarantined;
        }
    }

    /// Mark an instance as draining (graceful shutdown in progress)
    pub fn mark_draining(&self, gear: &str, instance_id: Uuid) {
        if let Some(mut vec) = self.inner.get_mut(gear)
            && let Some(inst) = vec.iter_mut().find(|i| i.instance_id == instance_id)
        {
            inst.inner.write().state = InstanceState::Draining;
        }
    }

    /// Remove an instance from the directory
    pub fn deregister(&self, gear: &str, instance_id: Uuid) {
        let mut remove_gear = false;
        {
            if let Some(mut vec) = self.inner.get_mut(gear) {
                let list = vec.value_mut();
                list.retain(|inst| inst.instance_id != instance_id);
                if list.is_empty() {
                    remove_gear = true;
                }
            }
        }

        if remove_gear {
            self.inner.remove(gear);
            self.rr_counters.remove(gear);
            self.rr_counters.remove(&format!("rest:{gear}"));
        }
    }

    /// Get all instances of a specific gear
    #[must_use]
    pub fn instances_of(&self, gear: &str) -> Vec<Arc<GearInstance>> {
        self.inner.get(gear).map(|v| v.clone()).unwrap_or_default()
    }

    /// Get all instances across all gears
    #[must_use]
    pub fn all_instances(&self) -> Vec<Arc<GearInstance>> {
        self.inner
            .iter()
            .flat_map(|entry| entry.value().clone())
            .collect()
    }

    /// Quarantine or evict stale instances based on heartbeat policy
    pub fn evict_stale(&self, now: Instant) {
        use InstanceState::{Draining, Quarantined};
        let mut empty_gears = Vec::new();

        for mut entry in self.inner.iter_mut() {
            let gear = entry.key().clone();
            let vec = entry.value_mut();
            vec.retain(|inst| {
                let state = inst.inner.read();
                let age = now.saturating_duration_since(state.last_heartbeat);

                // Quarantine instances that have exceeded TTL
                if age >= self.hb_ttl && !matches!(state.state, Quarantined | Draining) {
                    drop(state); // Release read lock before write
                    inst.inner.write().state = Quarantined;
                    return true; // Keep quarantined instances for now
                }

                // Evict quarantined instances that exceed grace period
                if state.state == Quarantined && age >= self.hb_ttl + self.hb_grace {
                    return false; // Remove from directory
                }

                true
            });

            if vec.is_empty() {
                empty_gears.push(gear);
            }
        }

        for gear in empty_gears {
            self.inner.remove(&gear);
            self.rr_counters.remove(&gear);
            self.rr_counters.remove(&format!("rest:{gear}"));
        }
    }

    /// From a candidate set, return the serving subset (`Healthy`/`Ready`); if
    /// none are serving, return the whole set so resolution falls back to the
    /// not-ready instances (`Registered`/`Quarantined`/`Draining`) rather than
    /// failing closed. `context` names the pool for the cold-path `debug!`.
    fn prefer_serving(candidates: Vec<Arc<GearInstance>>, context: &str) -> Vec<Arc<GearInstance>> {
        let serving: Vec<Arc<GearInstance>> = candidates
            .iter()
            .filter(|inst| matches!(inst.state(), InstanceState::Healthy | InstanceState::Ready))
            .cloned()
            .collect();
        if serving.is_empty() {
            tracing::debug!(
                context,
                "no serving (Ready/Healthy) instance available; round-robining over the \
                 not-ready set instead of returning None"
            );
            candidates
        } else {
            serving
        }
    }

    /// Pick an instance using round-robin selection, preferring healthy instances
    #[must_use]
    pub fn pick_instance_round_robin(&self, gear: &str) -> Option<Arc<GearInstance>> {
        let instances_entry = self.inner.get(gear)?;
        let instances = instances_entry.value();

        if instances.is_empty() {
            return None;
        }

        // Non-empty in => non-empty out, so `candidates` is guaranteed non-empty.
        let candidates = Self::prefer_serving(instances.clone(), gear);

        let len = candidates.len();
        let mut counter = self.rr_counters.entry(gear.to_owned()).or_insert(0);
        let idx = *counter % len;
        *counter = (*counter + 1) % len;

        candidates.get(idx).cloned()
    }

    /// Pick a service endpoint using round-robin, returning (gear, instance, endpoint).
    /// Prefers healthy/ready instances and automatically rotates among them.
    #[must_use]
    pub fn pick_service_round_robin(
        &self,
        service_name: &str,
    ) -> Option<(String, Arc<GearInstance>, Endpoint)> {
        // Collect the `Arc` of every instance that provides this service, then
        // apply the shared serving-preference fallback so gRPC resolution
        // survives an all-not-ready gear instead of failing closed. Cloning an
        // `Arc` is a refcount bump; the gear name and endpoint are cloned once,
        // for the winner, after the index is chosen.
        let mut providing: Vec<Arc<GearInstance>> = Vec::new();
        for entry in &self.inner {
            for inst in entry.value() {
                if inst.grpc_services.contains_key(service_name) {
                    providing.push(inst.clone());
                }
            }
        }

        if providing.is_empty() {
            return None;
        }

        let mut candidates = Self::prefer_serving(providing, service_name);

        // Use a counter keyed by service name for round-robin
        let len = candidates.len();
        let service_key = service_name.to_owned();
        let mut counter = self.rr_counters.entry(service_key).or_insert(0);
        let idx = *counter % len;
        *counter = (*counter + 1) % len;

        let inst = candidates.swap_remove(idx);
        let endpoint = inst.grpc_services.get(service_name)?.clone();
        let gear = inst.gear.clone();
        Some((gear, inst, endpoint))
    }

    /// Resolve a REST endpoint for a gear using round-robin over instances that
    /// expose one, preferring healthy/ready instances.
    #[must_use]
    pub fn pick_rest_endpoint_round_robin(&self, gear: &str) -> Option<Endpoint> {
        let instances_entry = self.inner.get(gear)?;
        let instances = instances_entry.value();

        // Only instances that actually expose a REST endpoint are candidates.
        let with_rest: Vec<_> = instances
            .iter()
            .filter(|inst| inst.rest_endpoint.is_some())
            .cloned()
            .collect();

        if with_rest.is_empty() {
            return None;
        }

        let candidates = Self::prefer_serving(with_rest, gear);

        let len = candidates.len();
        let rr_key = format!("rest:{gear}");
        let mut counter = self.rr_counters.entry(rr_key).or_insert(0);
        let idx = *counter % len;
        *counter = (*counter + 1) % len;

        candidates
            .get(idx)
            .and_then(|inst| inst.rest_endpoint.clone())
    }

    /// Return a gear that currently advertises the gRPC service `service_name`,
    /// if any (any instance state counts); in the misconfigured multi-owner case
    /// the first match in iteration order.
    ///
    /// Test-only: [`register_instance`](Self::register_instance) enforces single
    /// ownership with its own inline scan, so this is not part of the public
    /// surface — it exists purely to let the tests below assert who owns a name.
    #[cfg(test)]
    #[must_use]
    fn grpc_service_owner(&self, service_name: &str) -> Option<String> {
        for entry in &self.inner {
            if entry
                .value()
                .iter()
                .any(|inst| inst.grpc_services.contains_key(service_name))
            {
                return Some(entry.key().clone());
            }
        }
        None
    }

    /// Retrieve the `OpenAPI` spec of a gear, taken from the first registered
    /// instance that published one.
    #[must_use]
    pub fn openapi_spec_of(&self, gear: &str) -> Option<String> {
        let instances_entry = self.inner.get(gear)?;
        instances_entry
            .value()
            .iter()
            .find_map(|inst| inst.openapi_spec.clone())
    }
}

impl Default for GearManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use std::thread::sleep;
    use std::time::Duration;

    #[test]
    fn test_register_and_retrieve_instances() {
        let dir = GearManager::new();
        let instance_id = Uuid::new_v4();
        let instance = Arc::new(
            GearInstance::new("test_gear", instance_id)
                .with_control(Endpoint::http("localhost", 8080))
                .with_version("1.0.0"),
        );

        seed(&dir, instance);

        let instances = dir.instances_of("test_gear");
        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].instance_id, instance_id);
        assert_eq!(instances[0].gear, "test_gear");
        assert_eq!(instances[0].version, Some("1.0.0".to_owned()));
    }

    /// Seed the registry in a test. The fixture is assumed to advertise no
    /// conflicting gRPC service name, so a conflict is a bug in the test, not
    /// an expected outcome — surface it loudly rather than swallowing it.
    #[track_caller]
    fn seed(mgr: &GearManager, instance: Arc<GearInstance>) {
        mgr.register_instance(instance)
            .expect("test fixture must not create a gRPC service-name conflict");
    }

    fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn reregister_without_labels_preserves_stored_labels() {
        let dir = GearManager::new();
        let instance_id = Uuid::new_v4();

        // Initial registration carries a shard label.
        seed(
            &dir,
            Arc::new(
                GearInstance::new("shard-gear", instance_id).with_labels(labels(&[("shard", "7")])),
            ),
        );

        // A label-less re-registration (e.g. periodic self-heal / REST
        // augmentation) must NOT wipe the stored labels.
        seed(
            &dir,
            Arc::new(GearInstance::new("shard-gear", instance_id).with_version("2.0.0")),
        );

        let registered = dir.instances_of("shard-gear");
        assert_eq!(registered.len(), 1);
        assert_eq!(
            registered[0].labels.get("shard"),
            Some(&"7".to_owned()),
            "label-less re-registration must preserve stored labels"
        );
        assert_eq!(registered[0].version, Some("2.0.0".to_owned()));
    }

    #[test]
    fn reregister_with_labels_replaces_stored_labels() {
        let dir = GearManager::new();
        let instance_id = Uuid::new_v4();

        seed(
            &dir,
            Arc::new(
                GearInstance::new("shard-gear", instance_id).with_labels(labels(&[("shard", "7")])),
            ),
        );
        // An explicit non-empty set replaces the stored one wholesale.
        seed(
            &dir,
            Arc::new(
                GearInstance::new("shard-gear", instance_id).with_labels(labels(&[("shard", "8")])),
            ),
        );

        let registered = dir.instances_of("shard-gear");
        assert_eq!(registered.len(), 1);
        assert_eq!(registered[0].labels.get("shard"), Some(&"8".to_owned()));
    }

    #[test]
    fn test_register_multiple_instances() {
        let dir = GearManager::new();

        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        let instance1 = Arc::new(GearInstance::new("test_gear", id1));
        let instance2 = Arc::new(GearInstance::new("test_gear", id2));

        seed(&dir, instance1);
        seed(&dir, instance2);

        let registered = dir.instances_of("test_gear");
        assert_eq!(registered.len(), 2);

        let ids: Vec<_> = registered.iter().map(|i| i.instance_id).collect();
        assert!(ids.contains(&id1));
        assert!(ids.contains(&id2));
    }

    #[test]
    fn test_update_existing_instance() {
        let dir = GearManager::new();
        let instance_id = Uuid::new_v4();

        let initial_instance =
            Arc::new(GearInstance::new("test_gear", instance_id).with_version("1.0.0"));
        seed(&dir, initial_instance);

        let updated_instance =
            Arc::new(GearInstance::new("test_gear", instance_id).with_version("2.0.0"));
        seed(&dir, updated_instance);

        let registered = dir.instances_of("test_gear");
        assert_eq!(registered.len(), 1, "Should not duplicate instance");
        assert_eq!(registered[0].version, Some("2.0.0".to_owned()));
    }

    #[test]
    fn test_reregistration_preserves_liveness_state() {
        let dir = GearManager::new();
        let instance_id = Uuid::new_v4();

        // Register, then heartbeat so the instance is Healthy (routable).
        seed(
            &dir,
            Arc::new(GearInstance::new("test_gear", instance_id).with_version("1.0.0")),
        );
        dir.update_heartbeat("test_gear", instance_id, Instant::now());
        assert!(matches!(
            dir.instances_of("test_gear")[0].state(),
            InstanceState::Healthy
        ));

        // A periodic self-heal re-registration must NOT reset liveness back to
        // Registered — otherwise the instance drops out of gRPC round-robin
        // until the next heartbeat (the "split-brain" flap).
        seed(
            &dir,
            Arc::new(GearInstance::new("test_gear", instance_id).with_version("2.0.0")),
        );

        let instances = dir.instances_of("test_gear");
        assert_eq!(instances.len(), 1);
        assert!(
            matches!(instances[0].state(), InstanceState::Healthy),
            "re-registration must preserve the Healthy state"
        );
        assert_eq!(
            instances[0].version,
            Some("2.0.0".to_owned()),
            "re-registration must still refresh metadata/endpoints"
        );
    }

    #[test]
    fn test_concurrent_reregister_and_heartbeat_preserves_state() {
        let dir = GearManager::new();
        let instance_id = Uuid::new_v4();

        let initial = Arc::new(GearInstance::new("test_gear", instance_id).with_version("1.0.0"));
        seed(&dir, initial);
        dir.update_heartbeat("test_gear", instance_id, Instant::now());
        assert!(matches!(
            dir.instances_of("test_gear")[0].state(),
            InstanceState::Healthy
        ));

        let start = Instant::now();

        std::thread::scope(|s| {
            s.spawn(|| {
                for _ in 0..1000 {
                    dir.update_heartbeat("test_gear", instance_id, Instant::now());
                }
            });
            s.spawn(|| {
                for i in 0..1000 {
                    let version = if i % 2 == 0 { "2.0.0" } else { "3.0.0" };
                    let reinst = Arc::new(
                        GearInstance::new("test_gear", instance_id)
                            .with_version(version)
                            .with_rest_endpoint(Endpoint::http(
                                "127.0.0.1",
                                8000u16 + u16::try_from(i % 10).expect("i % 10 fits in u16"),
                            )),
                    );
                    seed(&dir, reinst);
                }
            });
        });

        let instances = dir.instances_of("test_gear");
        assert_eq!(instances.len(), 1);
        assert!(
            matches!(instances[0].state(), InstanceState::Healthy),
            "concurrent re-registration must not reset Healthy state"
        );
        assert!(
            instances[0].last_heartbeat() >= start,
            "concurrent re-registration must not lose heartbeat updates"
        );
    }

    #[test]
    fn test_mark_ready() {
        let dir = GearManager::new();
        let instance_id = Uuid::new_v4();
        let instance = Arc::new(GearInstance::new("test_gear", instance_id));

        seed(&dir, instance);

        dir.mark_ready("test_gear", instance_id);

        let instances = dir.instances_of("test_gear");
        assert_eq!(instances.len(), 1);
        assert!(matches!(instances[0].state(), InstanceState::Ready));
    }

    #[test]
    fn test_update_heartbeat() {
        let dir = GearManager::new();
        let instance_id = Uuid::new_v4();
        let instance = Arc::new(GearInstance::new("test_gear", instance_id));
        let initial_heartbeat = instance.last_heartbeat();

        seed(&dir, instance);

        // Sleep to ensure time difference
        sleep(Duration::from_millis(10));

        let new_heartbeat = Instant::now();
        dir.update_heartbeat("test_gear", instance_id, new_heartbeat);

        let instances = dir.instances_of("test_gear");
        assert!(instances[0].last_heartbeat() > initial_heartbeat);
        assert!(matches!(instances[0].state(), InstanceState::Healthy));
    }

    #[test]
    fn test_all_instances() {
        let dir = GearManager::new();

        let instance1 = Arc::new(GearInstance::new("gear_a", Uuid::new_v4()));
        let instance2 = Arc::new(GearInstance::new("gear_b", Uuid::new_v4()));
        let instance3 = Arc::new(GearInstance::new("gear_a", Uuid::new_v4()));

        seed(&dir, instance1);
        seed(&dir, instance2);
        seed(&dir, instance3);

        let all = dir.all_instances();
        assert_eq!(all.len(), 3);

        let gears: Vec<_> = all.iter().map(|i| i.gear.as_str()).collect();
        assert_eq!(gears.iter().filter(|&m| *m == "gear_a").count(), 2);
        assert_eq!(gears.iter().filter(|&m| *m == "gear_b").count(), 1);
    }

    #[test]
    fn test_pick_instance_round_robin() {
        let dir = GearManager::new();

        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        let instance1 = Arc::new(GearInstance::new("test_gear", id1));
        let instance2 = Arc::new(GearInstance::new("test_gear", id2));

        seed(&dir, instance1);
        seed(&dir, instance2);

        // Pick three times to verify round-robin behavior
        let picked1 = dir.pick_instance_round_robin("test_gear").unwrap();
        let picked2 = dir.pick_instance_round_robin("test_gear").unwrap();
        let picked3 = dir.pick_instance_round_robin("test_gear").unwrap();

        let ids = [
            picked1.instance_id,
            picked2.instance_id,
            picked3.instance_id,
        ];

        // With 2 instances, we expect round-robin pattern like A, B, A
        // Check that both instance IDs appear and that at least one repeats
        assert!(ids.contains(&id1));
        assert!(ids.contains(&id2));
        // First and third pick should be the same (round-robin wraps)
        assert_eq!(picked1.instance_id, picked3.instance_id);
        // Second pick should be different from the first
        assert_ne!(picked1.instance_id, picked2.instance_id);
    }

    #[test]
    fn test_pick_instance_none_available() {
        let dir = GearManager::new();
        let picked = dir.pick_instance_round_robin("nonexistent_gear");
        assert!(picked.is_none());
    }

    #[test]
    fn test_endpoint_creation() {
        let plain_ep = Endpoint::http("localhost", 8080);
        assert_eq!(plain_ep.uri, "http://localhost:8080");

        let secure_ep = Endpoint::https("localhost", 8443);
        assert_eq!(secure_ep.uri, "https://localhost:8443");

        let uds_ep = Endpoint::uds("/tmp/socket.sock");
        assert!(uds_ep.uri.starts_with("unix://"));
        assert!(uds_ep.uri.contains("socket.sock"));

        let custom_ep = Endpoint::from_uri("http://example.com");
        assert_eq!(custom_ep.uri, "http://example.com");
    }

    #[test]
    fn test_endpoint_kind() {
        let plain_ep = Endpoint::http("127.0.0.1", 8080);
        match plain_ep.kind() {
            EndpointKind::Tcp(addr) => {
                assert_eq!(addr.ip().to_string(), "127.0.0.1");
                assert_eq!(addr.port(), 8080);
            }
            _ => panic!("Expected TCP endpoint for http"),
        }

        let secure_ep = Endpoint::https("127.0.0.1", 8443);
        match secure_ep.kind() {
            EndpointKind::Tcp(addr) => {
                assert_eq!(addr.ip().to_string(), "127.0.0.1");
                assert_eq!(addr.port(), 8443);
            }
            _ => panic!("Expected TCP endpoint for https"),
        }

        let uds_ep = Endpoint::uds("/tmp/test.sock");
        match uds_ep.kind() {
            EndpointKind::Uds(path) => {
                assert!(path.to_string_lossy().contains("test.sock"));
            }
            _ => panic!("Expected UDS endpoint"),
        }

        let other_ep = Endpoint::from_uri("grpc://example.com");
        match other_ep.kind() {
            EndpointKind::Other(uri) => {
                assert_eq!(uri, "grpc://example.com");
            }
            _ => panic!("Expected Other endpoint"),
        }
    }

    #[test]
    fn test_gear_instance_builder() {
        let instance_id = Uuid::new_v4();
        let instance = GearInstance::new("test_gear", instance_id)
            .with_control(Endpoint::http("localhost", 8080))
            .with_version("1.2.3")
            .with_grpc_service("service1", Endpoint::http("localhost", 8082))
            .with_grpc_service("service2", Endpoint::http("localhost", 8083));

        assert_eq!(instance.gear, "test_gear");
        assert_eq!(instance.instance_id, instance_id);
        assert!(instance.control.is_some());
        assert_eq!(instance.version, Some("1.2.3".to_owned()));
        assert_eq!(instance.grpc_services.len(), 2);
        assert!(instance.grpc_services.contains_key("service1"));
        assert!(instance.grpc_services.contains_key("service2"));
        assert!(matches!(instance.state(), InstanceState::Registered));
    }

    #[test]
    fn test_quarantine_and_evict() {
        let ttl = Duration::from_millis(50);
        let grace = Duration::from_millis(50);
        let dir = GearManager::new().with_heartbeat_policy(ttl, grace);

        let now = Instant::now();
        let instance = GearInstance::new("test_gear", Uuid::new_v4());
        // Set the last heartbeat to be stale
        instance.inner.write().last_heartbeat = now
            .checked_sub(ttl)
            .and_then(|t| t.checked_sub(Duration::from_millis(10)))
            .expect("test duration subtraction should not underflow");

        seed(&dir, Arc::new(instance));

        dir.evict_stale(now);
        let instances = dir.instances_of("test_gear");
        assert_eq!(instances.len(), 1);
        assert!(matches!(instances[0].state(), InstanceState::Quarantined));

        let later = now + grace + Duration::from_millis(10);
        dir.evict_stale(later);

        let instances_after = dir.instances_of("test_gear");
        assert!(instances_after.is_empty());
    }

    #[test]
    fn test_instances_of_empty() {
        let dir = GearManager::new();
        let instances = dir.instances_of("nonexistent");
        assert!(instances.is_empty());
    }

    #[test]
    fn test_rr_prefers_healthy() {
        let dir = GearManager::new();

        // Create two instances: one healthy, one quarantined
        let healthy_id = Uuid::new_v4();
        let healthy = Arc::new(GearInstance::new("test_gear", healthy_id));
        seed(&dir, healthy);
        dir.update_heartbeat("test_gear", healthy_id, Instant::now());

        let quarantined_id = Uuid::new_v4();
        let quarantined = Arc::new(GearInstance::new("test_gear", quarantined_id));
        seed(&dir, quarantined);
        dir.mark_quarantined("test_gear", quarantined_id);

        // RR should only pick the healthy instance
        for _ in 0..5 {
            let picked = dir.pick_instance_round_robin("test_gear").unwrap();
            assert_eq!(picked.instance_id, healthy_id);
        }
    }

    #[test]
    fn test_pick_rest_endpoint_and_openapi() {
        let dir = GearManager::new();
        let id = Uuid::new_v4();
        let instance = Arc::new(
            GearInstance::new("billing", id)
                .with_rest_endpoint(Endpoint::http("billing", 8080))
                .with_openapi_spec("{\"openapi\":\"3.1.0\"}"),
        );
        seed(&dir, instance);

        let rest = dir.pick_rest_endpoint_round_robin("billing").unwrap();
        assert_eq!(rest.uri, "http://billing:8080");

        let spec = dir.openapi_spec_of("billing").unwrap();
        assert!(spec.contains("openapi"));
    }

    #[test]
    fn test_pick_rest_endpoint_none_when_absent() {
        let dir = GearManager::new();
        let id = Uuid::new_v4();
        // Instance exposes only a gRPC service, no REST endpoint.
        let instance = Arc::new(
            GearInstance::new("grpc_only", id)
                .with_grpc_service("some.Service", Endpoint::http("127.0.0.1", 9000)),
        );
        seed(&dir, instance);

        assert!(dir.pick_rest_endpoint_round_robin("grpc_only").is_none());
        assert!(dir.openapi_spec_of("grpc_only").is_none());
        assert!(dir.pick_rest_endpoint_round_robin("missing").is_none());
    }

    #[test]
    fn test_pick_rest_endpoint_round_robin_rotates() {
        let dir = GearManager::new();
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        let inst1 = Arc::new(
            GearInstance::new("web", id1).with_rest_endpoint(Endpoint::http("127.0.0.1", 8001)),
        );
        let inst2 = Arc::new(
            GearInstance::new("web", id2).with_rest_endpoint(Endpoint::http("127.0.0.1", 8002)),
        );
        seed(&dir, inst1);
        seed(&dir, inst2);
        dir.update_heartbeat("web", id1, Instant::now());
        dir.update_heartbeat("web", id2, Instant::now());

        let ep1 = dir.pick_rest_endpoint_round_robin("web").unwrap();
        let ep2 = dir.pick_rest_endpoint_round_robin("web").unwrap();
        let ep3 = dir.pick_rest_endpoint_round_robin("web").unwrap();

        assert_ne!(ep1, ep2);
        assert_eq!(ep1, ep3);
    }

    #[test]
    fn test_pick_service_round_robin() {
        let dir = GearManager::new();

        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        // Register two instances providing the same service
        let inst1 = Arc::new(
            GearInstance::new("test_gear", id1)
                .with_grpc_service("test.Service", Endpoint::http("127.0.0.1", 8001)),
        );
        let inst2 = Arc::new(
            GearInstance::new("test_gear", id2)
                .with_grpc_service("test.Service", Endpoint::http("127.0.0.1", 8002)),
        );

        seed(&dir, inst1);
        seed(&dir, inst2);

        // Mark both as healthy
        dir.update_heartbeat("test_gear", id1, Instant::now());
        dir.update_heartbeat("test_gear", id2, Instant::now());

        // Pick should rotate between instances
        let pick1 = dir.pick_service_round_robin("test.Service");
        let pick2 = dir.pick_service_round_robin("test.Service");
        let pick3 = dir.pick_service_round_robin("test.Service");

        assert!(pick1.is_some());
        assert!(pick2.is_some());
        assert!(pick3.is_some());

        let (_, inst1, ep1) = pick1.unwrap();
        let (_, inst2, ep2) = pick2.unwrap();
        let (_, inst3, _) = pick3.unwrap();

        // First and third should be the same (round-robin)
        assert_eq!(inst1.instance_id, inst3.instance_id);
        // First and second should be different
        assert_ne!(inst1.instance_id, inst2.instance_id);
        // Endpoints should differ
        assert_ne!(ep1, ep2);
    }

    #[test]
    fn pick_service_falls_back_to_not_ready() {
        // A gear whose only service-providing instance is not yet serving
        // (Registered, no heartbeat) must still resolve, mirroring the REST and
        // instance pickers' not-ready fallback rather than failing closed.
        let dir = GearManager::new();
        let id = Uuid::new_v4();
        seed(
            &dir,
            Arc::new(
                GearInstance::new("worker", id)
                    .with_grpc_service("worker.Svc", Endpoint::http("127.0.0.1", 9000)),
            ),
        );
        // No heartbeat: the instance stays Registered (not serving).
        assert!(matches!(
            dir.instances_of("worker")[0].state(),
            InstanceState::Registered
        ));

        let picked = dir.pick_service_round_robin("worker.Svc");
        assert!(
            picked.is_some(),
            "gRPC service resolution must fall back to the not-ready instance"
        );
        let (gear, _, ep) = picked.unwrap();
        assert_eq!(gear, "worker");
        assert_eq!(ep, Endpoint::http("127.0.0.1", 9000));
    }

    #[test]
    fn grpc_service_owner_reports_owning_gear_regardless_of_health() {
        let dir = GearManager::new();

        // A freshly-registered (not-yet-serving) instance still owns its name.
        seed(
            &dir,
            Arc::new(
                GearInstance::new("authz-resolver", Uuid::new_v4()).with_grpc_service(
                    "cf.authz.v1.AuthzService",
                    Endpoint::http("127.0.0.1", 9000),
                ),
            ),
        );

        assert_eq!(
            dir.grpc_service_owner("cf.authz.v1.AuthzService")
                .as_deref(),
            Some("authz-resolver"),
            "the advertising gear owns the name even while only Registered"
        );
        assert!(
            dir.grpc_service_owner("unowned.Service").is_none(),
            "an unadvertised name has no owner"
        );
    }

    #[test]
    fn deregister_releases_grpc_service_name_to_another_gear() {
        let dir = GearManager::new();
        let service = "cf.authz.v1.AuthzService";
        let owner_id = Uuid::new_v4();

        seed(
            &dir,
            Arc::new(
                GearInstance::new("authz-resolver", owner_id)
                    .with_grpc_service(service, Endpoint::http("127.0.0.1", 9000)),
            ),
        );

        // While the owner is registered, a different gear cannot claim the name.
        dir.register_instance(Arc::new(
            GearInstance::new("successor", Uuid::new_v4())
                .with_grpc_service(service, Endpoint::http("127.0.0.1", 9001)),
        ))
        .unwrap_err();

        // Deregistering the owner releases the name...
        dir.deregister("authz-resolver", owner_id);
        assert!(
            dir.grpc_service_owner(service).is_none(),
            "the name is unowned once its owner deregisters"
        );

        // ...so a different gear may now claim it.
        dir.register_instance(Arc::new(
            GearInstance::new("successor", Uuid::new_v4())
                .with_grpc_service(service, Endpoint::http("127.0.0.1", 9001)),
        ))
        .expect("a released gRPC service name must be claimable by another gear");
        assert_eq!(
            dir.grpc_service_owner(service).as_deref(),
            Some("successor")
        );
    }

    #[test]
    fn eviction_releases_grpc_service_name_only_after_full_evict() {
        let ttl = Duration::from_millis(50);
        let grace = Duration::from_millis(50);
        let dir = GearManager::new().with_heartbeat_policy(ttl, grace);
        let service = "cf.authz.v1.AuthzService";

        let now = Instant::now();
        let owner = GearInstance::new("authz-resolver", Uuid::new_v4())
            .with_grpc_service(service, Endpoint::http("127.0.0.1", 9000));
        // Stale from the start so the first eviction pass quarantines it.
        owner.inner.write().last_heartbeat = now
            .checked_sub(ttl)
            .and_then(|t| t.checked_sub(Duration::from_millis(10)))
            .expect("test duration subtraction should not underflow");
        seed(&dir, Arc::new(owner));

        // First pass only *quarantines* the dead gear — it is not yet evicted,
        // so it still owns the name and a successor is rejected. A dead gear
        // keeps blocking its gRPC name (regardless of health) until fully
        // evicted, i.e. for hb_ttl + hb_grace.
        dir.evict_stale(now);
        assert!(matches!(
            dir.instances_of("authz-resolver")[0].state(),
            InstanceState::Quarantined
        ));
        let conflict = dir
            .register_instance(Arc::new(
                GearInstance::new("successor", Uuid::new_v4())
                    .with_grpc_service(service, Endpoint::http("127.0.0.1", 9001)),
            ))
            .unwrap_err();
        assert_eq!(
            conflict.owner, "authz-resolver",
            "a quarantined-but-not-evicted owner still holds the name"
        );

        // Second pass past the grace period fully evicts it, releasing the name
        // so another gear may take it over.
        dir.evict_stale(now + grace + Duration::from_millis(10));
        assert!(
            dir.grpc_service_owner(service).is_none(),
            "eviction hands the name back once the grace period lapses"
        );
        dir.register_instance(Arc::new(
            GearInstance::new("successor", Uuid::new_v4())
                .with_grpc_service(service, Endpoint::http("127.0.0.1", 9002)),
        ))
        .expect("an evicted owner's gRPC name must be claimable by another gear");
        assert_eq!(
            dir.grpc_service_owner(service).as_deref(),
            Some("successor")
        );
    }

    #[test]
    fn test_deregister_clears_rr_counters() {
        let dir = GearManager::new();
        let id = Uuid::new_v4();
        let instance = Arc::new(
            GearInstance::new("web", id).with_rest_endpoint(Endpoint::http("127.0.0.1", 8001)),
        );
        seed(&dir, instance);
        dir.update_heartbeat("web", id, Instant::now());

        // Exercise both round-robin counters so the keys are created.
        assert!(dir.pick_instance_round_robin("web").is_some());
        assert!(dir.pick_rest_endpoint_round_robin("web").is_some());

        assert!(dir.rr_counters.contains_key("web"));
        assert!(dir.rr_counters.contains_key("rest:web"));

        dir.deregister("web", id);

        assert!(!dir.rr_counters.contains_key("web"));
        assert!(!dir.rr_counters.contains_key("rest:web"));
    }

    #[test]
    fn test_evict_stale_clears_rr_counters() {
        let ttl = Duration::from_millis(50);
        let grace = Duration::from_millis(50);
        let dir = GearManager::new().with_heartbeat_policy(ttl, grace);

        let now = Instant::now();
        let id = Uuid::new_v4();
        let instance = Arc::new(
            GearInstance::new("web", id).with_rest_endpoint(Endpoint::http("127.0.0.1", 8001)),
        );
        // Set the last heartbeat to be stale so the instance is quarantined then evicted.
        instance.inner.write().last_heartbeat = now
            .checked_sub(ttl)
            .and_then(|t| t.checked_sub(Duration::from_millis(10)))
            .expect("test duration subtraction should not underflow");

        seed(&dir, instance);
        assert!(dir.pick_rest_endpoint_round_robin("web").is_some());

        assert!(dir.rr_counters.contains_key("rest:web"));

        // First eviction pass quarantines the stale instance.
        dir.evict_stale(now);
        let instances = dir.instances_of("web");
        assert_eq!(instances.len(), 1);
        assert!(matches!(instances[0].state(), InstanceState::Quarantined));

        // Second pass evicts quarantined instances that exceeded the grace period.
        let later = now + grace + Duration::from_millis(10);
        dir.evict_stale(later);

        assert!(dir.instances_of("web").is_empty());
        assert!(!dir.rr_counters.contains_key("web"));
        assert!(!dir.rr_counters.contains_key("rest:web"));
    }

    #[test]
    fn register_instance_rejects_cross_gear_grpc_name() {
        let mgr = GearManager::new();
        let service = "cf.authz.v1.AuthzService";

        mgr.register_instance(Arc::new(
            GearInstance::new("authz-resolver", Uuid::new_v4())
                .with_grpc_service(service, Endpoint::http("127.0.0.1", 9000)),
        ))
        .expect("claiming an unowned service name must succeed");

        // A different gear claiming the same name is rejected, store untouched.
        let conflict = mgr
            .register_instance(Arc::new(
                GearInstance::new("evil", Uuid::new_v4())
                    .with_grpc_service(service, Endpoint::http("127.0.0.1", 9001)),
            ))
            .unwrap_err();
        assert_eq!(conflict.service_name, service);
        assert_eq!(conflict.owner, "authz-resolver");
        assert!(mgr.instances_of("evil").is_empty());

        // The owning gear may add another instance under the same name.
        mgr.register_instance(Arc::new(
            GearInstance::new("authz-resolver", Uuid::new_v4())
                .with_grpc_service(service, Endpoint::http("127.0.0.1", 9002)),
        ))
        .expect("the owning gear may add another instance for its own service");
    }

    #[test]
    fn declared_ownership_beats_registration_order() {
        let mgr = GearManager::new();
        let service = "cf.authz.v1.AuthzService";

        // Config declares the name is owned by `authz`.
        mgr.set_grpc_service_owners(HashMap::from([(service.to_owned(), "authz".to_owned())]));

        // A squatter registering *first* cannot claim a declared name; it is
        // rejected with the declared owner, and the store is untouched.
        let conflict = mgr
            .register_instance(Arc::new(
                GearInstance::new("evil", Uuid::new_v4())
                    .with_grpc_service(service, Endpoint::http("127.0.0.1", 9001)),
            ))
            .unwrap_err();
        assert_eq!(conflict.service_name, service);
        assert_eq!(conflict.owner, "authz");
        assert!(mgr.instances_of("evil").is_empty());

        // The declared owner is always admitted, even coming in second.
        mgr.register_instance(Arc::new(
            GearInstance::new("authz", Uuid::new_v4())
                .with_grpc_service(service, Endpoint::http("127.0.0.1", 9000)),
        ))
        .expect("the declared owner must be admitted");
        assert_eq!(mgr.grpc_service_owner(service).as_deref(), Some("authz"));
    }

    #[test]
    fn names_absent_from_declared_map_keep_first_registration_ownership() {
        let mgr = GearManager::new();
        // The map declares one name but says nothing about `worker.Svc`.
        mgr.set_grpc_service_owners(HashMap::from([(
            "cf.authz.v1.AuthzService".to_owned(),
            "authz".to_owned(),
        )]));
        let service = "worker.Svc";

        mgr.register_instance(Arc::new(
            GearInstance::new("worker-a", Uuid::new_v4())
                .with_grpc_service(service, Endpoint::http("127.0.0.1", 9000)),
        ))
        .expect("claiming an undeclared, unowned name must succeed");

        let conflict = mgr
            .register_instance(Arc::new(
                GearInstance::new("worker-b", Uuid::new_v4())
                    .with_grpc_service(service, Endpoint::http("127.0.0.1", 9001)),
            ))
            .unwrap_err();
        assert_eq!(conflict.owner, "worker-a");
    }

    /// Under contention, the atomic check+insert admits exactly one owner for a
    /// gRPC service name: N gears race to claim the same unowned name and only
    /// one commits, closing the check-then-write race the per-entry `DashMap`
    /// locks leave open.
    #[test]
    fn concurrent_register_admits_exactly_one_owner() {
        use std::sync::Barrier;

        let mgr = Arc::new(GearManager::new());
        let service = "cf.authz.v1.AuthzService";
        let racers = 16;
        let barrier = Arc::new(Barrier::new(racers));

        #[expect(
            clippy::needless_collect,
            reason = "a lazy iterator would join before all racers spawn, deadlocking the Barrier"
        )]
        let handles: Vec<_> = (0..racers)
            .map(|i| {
                let mgr = Arc::clone(&mgr);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let gear = format!("gear-{i}");
                    let instance = Arc::new(
                        GearInstance::new(gear.clone(), Uuid::new_v4()).with_grpc_service(
                            service,
                            Endpoint::http("127.0.0.1", 9000 + u16::try_from(i).unwrap()),
                        ),
                    );
                    // Release all threads at once to maximise contention.
                    barrier.wait();
                    (gear, mgr.register_instance(instance))
                })
            })
            .collect();

        let results: Vec<(String, Result<(), GrpcServiceNameConflict>)> = handles
            .into_iter()
            .map(|h| h.join().expect("registration thread must not panic"))
            .collect();

        // Exactly one gear wins the race.
        let winners: Vec<&str> = results
            .iter()
            .filter(|(_, r)| r.is_ok())
            .map(|(gear, _)| gear.as_str())
            .collect();
        assert_eq!(
            winners.len(),
            1,
            "exactly one competing registration must win"
        );
        let winner = winners[0];

        // Every loser is rejected with a conflict naming the winning gear — the
        // `owner` surfaced in the server-side warn log and the operator's
        // remediation hint, so it must be the gear that actually committed, not
        // just any error.
        for (gear, result) in &results {
            if gear == winner {
                continue;
            }
            let conflict = result
                .as_ref()
                .expect_err("a losing registration must be rejected, not silently dropped");
            assert_eq!(conflict.service_name, service);
            assert_eq!(
                conflict.owner.as_str(),
                winner,
                "every loser's conflict must name the single winning gear as owner"
            );
        }

        // ...and the store resolves the contested name to exactly that winner.
        assert_eq!(
            mgr.grpc_service_owner(service).as_deref(),
            Some(winner),
            "the contested name must resolve to the one gear that won"
        );
    }
}
