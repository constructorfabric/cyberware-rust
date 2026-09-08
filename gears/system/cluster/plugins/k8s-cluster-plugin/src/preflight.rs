//! Startup preflight (DESIGN.md §3.4, §6.7).
//!
//! `build_and_start` issues a bounded, read-only set of requests before returning,
//! turning every predictable runtime failure into a *startup* failure an operator
//! can act on, rather than a `403` on the first background renewal minutes later:
//!
//! 1. **Server version** (`GET /version`) — recorded on the startup log line;
//!    unreadable is a WARN, never fatal (nothing in v1 is version-gated).
//! 2. **RBAC probe** — one `SelfSubjectAccessReview` per `(verb, resource)` the
//!    *enabled* primitives need (§7). A denial fails startup naming the exact verb;
//!    the SSAR itself being denied is a degrade-with-WARN, and `skip_rbac_preflight`
//!    is the explicit escape hatch.
//! 3. **CRD canary** (cache only) — a create-then-delete of a **per-instance**
//!    `ClusterCacheEntry` that proves in one shot the CRD is installed *and* its
//!    served schema accepts what this plugin version writes (§6.7).
//!
//! The decision logic — which `(verb, resource)` each primitive needs, the
//! per-instance canary key, and how a denial reads back to an operator — is factored
//! into pure functions ([`probe_set`], [`canary_key`], [`rbac_failure_message`],
//! [`probe_denied`]) that the L1 suite covers exhaustively; the async wrappers do
//! only the I/O.

use std::collections::BTreeSet;

use cluster_sdk::ClusterError;
use k8s_openapi::api::authorization::v1::{
    ResourceAttributes, SelfSubjectAccessReview, SelfSubjectAccessReviewSpec,
};
use kube::Api;
use tracing::warn;

use crate::crd::{ClusterCacheEntry, ClusterCacheEntrySpec};
use crate::k8s_error::map_kube_error;
use crate::naming;

/// The `apiGroup` of `coordination.k8s.io/v1.Lease` (leader election, lock).
const GROUP_COORDINATION: &str = "coordination.k8s.io";
/// The `apiGroup` of this plugin's `ClusterCacheEntry` custom resource (cache).
const GROUP_CACHE: &str = "cluster.cf-gears.io";
/// The `leases` resource name.
const RESOURCE_LEASES: &str = "leases";
/// The `clustercacheentries` resource name.
const RESOURCE_CACHE: &str = "clustercacheentries";

/// The canary object's time-to-live: it is deleted before `build_and_start`
/// returns, and a leftover from a crash-between-create-and-delete is already past
/// this deadline, so the next cache instance's sweeper reclaims it (§3.4).
const CANARY_TTL_SECONDS: i64 = 60;

/// A native primitive, used to scope the RBAC probe set to exactly what is enabled
/// (§3.4): an operator granting the minimum for one primitive is never told to grant
/// the others' verbs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Primitive {
    /// Leader election over `leases`.
    LeaderElection,
    /// Distributed lock over `leases` (adds the reaper's `list`/`delete`).
    Lock,
    /// Cache over `clustercacheentries`.
    Cache,
}

/// One `(group, resource, verb)` the RBAC probe checks — the unit of a
/// `SelfSubjectAccessReview` (§3.4, §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Probe {
    /// The resource's API group.
    pub group: &'static str,
    /// The resource (plural).
    pub resource: &'static str,
    /// The verb being checked.
    pub verb: &'static str,
}

impl Primitive {
    /// The `(group, resource, verb)` set this primitive needs (§7).
    fn probes(self) -> &'static [Probe] {
        // Leader election: get/create/update/watch on leases.
        const LEADER: &[Probe] = &leases(&["get", "create", "update", "watch"]);
        // Lock: leader's set plus list/delete for the reaper (§5.5).
        const LOCK: &[Probe] = &leases(&["get", "create", "update", "list", "watch", "delete"]);
        // Cache: all six verbs on clustercacheentries.
        const CACHE: &[Probe] = &cache(&["get", "create", "update", "list", "watch", "delete"]);
        match self {
            Self::LeaderElection => LEADER,
            Self::Lock => LOCK,
            Self::Cache => CACHE,
        }
    }
}

/// Builds a `leases` probe list at compile time.
const fn leases<const N: usize>(verbs: &[&'static str; N]) -> [Probe; N] {
    let mut out = [Probe {
        group: GROUP_COORDINATION,
        resource: RESOURCE_LEASES,
        verb: "",
    }; N];
    let mut i = 0;
    while i < N {
        out[i].verb = verbs[i];
        i += 1;
    }
    out
}

/// Builds a `clustercacheentries` probe list at compile time.
const fn cache<const N: usize>(verbs: &[&'static str; N]) -> [Probe; N] {
    let mut out = [Probe {
        group: GROUP_CACHE,
        resource: RESOURCE_CACHE,
        verb: "",
    }; N];
    let mut i = 0;
    while i < N {
        out[i].verb = verbs[i];
        i += 1;
    }
    out
}

/// The deduplicated, ordered probe set for the enabled `primitives` (§3.4).
///
/// The union across primitives is deduplicated — leader election and lock both need
/// `get`/`leases`, and that is probed once — and ordered deterministically, so the
/// probe sequence and any failure message are stable.
#[must_use]
pub fn probe_set(primitives: &[Primitive]) -> Vec<Probe> {
    let mut set = BTreeSet::new();
    for primitive in primitives {
        set.extend(primitive.probes().iter().copied());
    }
    set.into_iter().collect()
}

/// The per-instance canary object name: `<prefix>-ca-preflight-<identity-hash16>`
/// (§3.4).
///
/// Keyed on the resolved identity's hash rather than a fixed constant, so two
/// instances starting at once do not collide on one object — each writes and
/// validates *its own* schema view. A shared constant key would leave every instance
/// but the create-winner reading a `409 AlreadyExists` and trusting a schema it
/// never exercised.
#[must_use]
pub fn canary_key(prefix: &str, identity: &str) -> String {
    format!("{prefix}-ca-preflight-{}", naming::hash16(identity))
}

/// Whether a `SelfSubjectAccessReview` response denies the probed access.
///
/// Access is granted only when the review is explicitly `allowed` and not
/// explicitly `denied`; a review with neither (or a missing status) is treated as a
/// denial, so an ambiguous answer fails closed rather than silently proceeding
/// toward a `403` at runtime.
#[must_use]
pub fn probe_denied(review: &SelfSubjectAccessReview) -> bool {
    match &review.status {
        Some(status) => !status.allowed || status.denied == Some(true),
        None => true,
    }
}

/// The `InvalidConfig` message for an RBAC preflight denial (§3.4), naming the
/// namespace, the denied `(verb, resource)` pairs, and where the Role lives.
#[must_use]
pub fn rbac_failure_message(namespace: &str, denied: &[Probe]) -> String {
    let mut verbs = denied
        .iter()
        .map(|p| format!("`{}` `{}`", p.verb, p.resource))
        .collect::<Vec<_>>();
    verbs.dedup();
    format!(
        "kubernetes RBAC insufficient in namespace `{namespace}`: this service account may not {}. \
         Grant the Role in plugins/k8s-cluster-plugin/docs/DESIGN.md section 7",
        verbs.join(", ")
    )
}

/// Builds the `SelfSubjectAccessReview` request for `probe` in `namespace`.
fn ssar_for(probe: Probe, namespace: &str) -> SelfSubjectAccessReview {
    SelfSubjectAccessReview {
        spec: SelfSubjectAccessReviewSpec {
            resource_attributes: Some(ResourceAttributes {
                namespace: Some(namespace.to_owned()),
                group: Some(probe.group.to_owned()),
                resource: Some(probe.resource.to_owned()),
                verb: Some(probe.verb.to_owned()),
                ..ResourceAttributes::default()
            }),
            ..SelfSubjectAccessReviewSpec::default()
        },
        ..SelfSubjectAccessReview::default()
    }
}

/// Runs the RBAC preflight for `primitives` (§3.4).
///
/// Returns `Ok(())` when every needed verb is allowed, when `skip_rbac_preflight`
/// is set, or when the SSAR itself is refused (a hardened cluster) — the last of
/// which logs `cluster.provider.rbac_unverified` and proceeds, because refusing to
/// start over an unavailable *diagnostic* would make the plugin unusable where it
/// would otherwise work.
///
/// # Errors
///
/// [`ClusterError::InvalidConfig`] naming the denied verbs when a probe comes back
/// denied.
pub async fn check_rbac(
    client: &kube::Client,
    namespace: &str,
    primitives: &[Primitive],
    skip_rbac_preflight: bool,
) -> Result<(), ClusterError> {
    if skip_rbac_preflight {
        return Ok(());
    }
    let api: Api<SelfSubjectAccessReview> = Api::all(client.clone());
    let mut denied = Vec::new();
    for probe in probe_set(primitives) {
        match api
            .create(
                &kube::api::PostParams::default(),
                &ssar_for(probe, namespace),
            )
            .await
        {
            Ok(review) => {
                if probe_denied(&review) {
                    denied.push(probe);
                }
            }
            // The SSAR itself was refused (e.g. an admission webhook, a cluster that
            // withholds `system:basic-user`): degrade rather than fail (§3.4). But
            // `break` instead of returning, so any denials already collected still
            // fail startup below — a known-missing verb must not be masked by a later
            // probe erroring, or the plugin would 403 on its first background op.
            Err(err) => {
                warn!(
                    error = %map_kube_error(&err),
                    "cluster.provider.rbac_unverified: a SelfSubjectAccessReview RBAC probe was \
                     refused; proceeding without full RBAC verification (set skip_rbac_preflight \
                     to silence this)"
                );
                break;
            }
        }
    }
    if denied.is_empty() {
        Ok(())
    } else {
        Err(ClusterError::InvalidConfig {
            reason: rbac_failure_message(namespace, &denied),
        })
    }
}

/// Reads the server version for the startup log line (§3.4); never fatal.
///
/// Returns the `gitVersion` string, or `None` (with a WARN) when `/version` is
/// unreadable — nothing in v1 is version-gated, so an unknown version proceeds.
pub async fn server_version(client: &kube::Client) -> Option<String> {
    match client.apiserver_version().await {
        Ok(info) => Some(info.git_version),
        Err(err) => {
            warn!(
                error = %map_kube_error(&err),
                "cluster.provider.version_unreadable: could not read the API server version; \
                 proceeding (nothing is version-gated)"
            );
            None
        }
    }
}

/// The absolute RFC 3339 expiry the canary is written with — [`CANARY_TTL_SECONDS`]
/// from now on the writer's clock (the documented writer-clock exception, §6.2).
fn canary_expires_at() -> String {
    let deadline = k8s_openapi::jiff::Timestamp::now()
        .checked_add(k8s_openapi::jiff::SignedDuration::from_secs(
            CANARY_TTL_SECONDS,
        ))
        .unwrap_or_else(|_| k8s_openapi::jiff::Timestamp::now());
    deadline.to_string()
}

/// Runs the cache CRD canary (§3.4, §6.7): create a per-instance
/// `ClusterCacheEntry`, then delete it, proving the CRD is installed and its served
/// schema accepts this plugin's spec shape.
///
/// # Errors
///
/// [`ClusterError::InvalidConfig`] naming `deploy/crd.yaml` when the CRD is not
/// served (`404` on the resource) or its schema rejects the spec (`422`) — both are
/// operator actions on a manifest, not backend faults (§10). Other errors map
/// through [`map_kube_error`].
pub async fn cache_canary(
    client: &kube::Client,
    namespace: &str,
    prefix: &str,
    identity: &str,
) -> Result<(), ClusterError> {
    let api: Api<ClusterCacheEntry> = Api::namespaced(client.clone(), namespace);
    let name = canary_key(prefix, identity);
    let spec = ClusterCacheEntrySpec::new(b"preflight", 1, Some(canary_expires_at()));
    let mut object = ClusterCacheEntry::new(&name, spec);
    object.metadata.namespace = Some(namespace.to_owned());

    match api.create(&kube::api::PostParams::default(), &object).await {
        Ok(_) => {}
        // A `409 AlreadyExists` here is a leftover canary from a crash between the
        // create and the delete below: `canary_key` is deterministic per
        // (prefix, identity), so it belongs to *this* identity. Reclaim it and
        // retry once — the doc's "a sweeper reclaims it" does not apply, because the
        // canary runs before `K8sCache::new`, so this instance has no sweeper yet
        // and a single-replica deployment could never restart otherwise (§3.4).
        Err(kube::Error::Api(status)) if status.code == 409 => {
            let _reclaimed = api.delete(&name, &kube::api::DeleteParams::default()).await;
            api.create(&kube::api::PostParams::default(), &object)
                .await
                .map_err(|err| map_canary_error(&err))?;
        }
        Err(err) => return Err(map_canary_error(&err)),
    }

    // Best-effort delete: an unconditional delete of an object we just created is
    // fine here (no guarded precondition), and a leftover is swept anyway (§3.4).
    if let Err(err) = api.delete(&name, &kube::api::DeleteParams::default()).await {
        warn!(
            error = %map_kube_error(&err),
            canary = %name,
            "cluster.provider.canary_cleanup_failed: could not delete the preflight canary; it \
             will be reclaimed by a cache sweeper once its TTL elapses"
        );
    }
    Ok(())
}

/// Maps a canary create error, translating the CRD-absent (`404`) and schema-skew
/// (`422`) cases to an actionable [`ClusterError::InvalidConfig`] naming
/// `deploy/crd.yaml` (§6.7, §10).
fn map_canary_error(err: &kube::Error) -> ClusterError {
    if let kube::Error::Api(status) = err
        && (status.code == 404 || status.code == 422)
    {
        return ClusterError::InvalidConfig {
            reason: format!(
                "kubernetes cache backend requires the ClusterCacheEntry CRD: \
                 clustercacheentries.cluster.cf-gears.io is not served or its schema rejects \
                 this plugin's spec (API server said: {}). Apply \
                 plugins/k8s-cluster-plugin/deploy/crd.yaml (cluster-admin, once per cluster), \
                 then restart",
                status.message
            ),
        };
    }
    map_kube_error(err)
}

#[cfg(test)]
mod tests {
    use super::{
        GROUP_CACHE, GROUP_COORDINATION, Primitive, Probe, RESOURCE_CACHE, RESOURCE_LEASES,
        canary_key, probe_denied, probe_set, rbac_failure_message,
    };
    use k8s_openapi::api::authorization::v1::SelfSubjectAccessReview;
    use k8s_openapi::api::authorization::v1::SubjectAccessReviewStatus;

    fn verbs_for(primitives: &[Primitive], group: &str, resource: &str) -> Vec<&'static str> {
        probe_set(primitives)
            .into_iter()
            .filter(|p| p.group == group && p.resource == resource)
            .map(|p| p.verb)
            .collect()
    }

    #[test]
    fn leader_election_probes_exactly_its_verbs_on_leases_only() {
        let probes = probe_set(&[Primitive::LeaderElection]);
        assert_eq!(
            verbs_for(
                &[Primitive::LeaderElection],
                GROUP_COORDINATION,
                RESOURCE_LEASES
            ),
            vec!["create", "get", "update", "watch"],
        );
        // Never asks about the cache resource.
        assert!(probes.iter().all(|p| p.resource == RESOURCE_LEASES));
        // Never asks for delete/list, which only the lock's reaper needs.
        assert!(
            probes
                .iter()
                .all(|p| p.verb != "delete" && p.verb != "list")
        );
    }

    #[test]
    fn lock_probes_add_list_and_delete_on_leases() {
        assert_eq!(
            verbs_for(&[Primitive::Lock], GROUP_COORDINATION, RESOURCE_LEASES),
            vec!["create", "delete", "get", "list", "update", "watch"],
        );
        assert!(
            probe_set(&[Primitive::Lock])
                .iter()
                .all(|p| p.resource == RESOURCE_LEASES)
        );
    }

    #[test]
    fn cache_probes_all_six_verbs_on_the_cache_resource_only() {
        let probes = probe_set(&[Primitive::Cache]);
        assert_eq!(
            verbs_for(&[Primitive::Cache], GROUP_CACHE, RESOURCE_CACHE),
            vec!["create", "delete", "get", "list", "update", "watch"],
        );
        assert!(probes.iter().all(|p| p.resource == RESOURCE_CACHE));
    }

    #[test]
    fn combined_primitives_dedup_shared_leases_probes() {
        // Leader + lock both need get/create/update/watch on leases; the union
        // dedups them (lock's set is a superset), so no verb appears twice.
        let both = probe_set(&[Primitive::LeaderElection, Primitive::Lock]);
        let lock_only = probe_set(&[Primitive::Lock]);
        assert_eq!(
            both, lock_only,
            "leader union lock == lock (leader subset of lock on leases)"
        );

        // All three: the lock's leases set plus the cache's own set, no dupes.
        let all = probe_set(&[Primitive::LeaderElection, Primitive::Lock, Primitive::Cache]);
        let mut deduped = all.clone();
        deduped.dedup();
        assert_eq!(all, deduped, "no probe appears twice");
        assert_eq!(
            all.len(),
            lock_only.len() + probe_set(&[Primitive::Cache]).len()
        );
    }

    #[test]
    fn canary_key_is_per_instance_and_stable() {
        let a = canary_key("cluster", "broker-7");
        let b = canary_key("cluster", "broker-8");
        assert_ne!(a, b, "distinct identities -> distinct canary objects");
        assert_eq!(
            a,
            canary_key("cluster", "broker-7"),
            "stable for one identity"
        );
        assert!(a.starts_with("cluster-ca-preflight-"));
        // The suffix is the 16-hex identity hash.
        let hash = a.rsplit('-').next().unwrap();
        assert_eq!(hash.len(), 16);
        assert!(
            hash.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
    }

    fn review(allowed: bool, denied: Option<bool>) -> SelfSubjectAccessReview {
        SelfSubjectAccessReview {
            status: Some(SubjectAccessReviewStatus {
                allowed,
                denied,
                ..SubjectAccessReviewStatus::default()
            }),
            ..SelfSubjectAccessReview::default()
        }
    }

    #[test]
    fn probe_denied_fails_closed() {
        assert!(
            !probe_denied(&review(true, None)),
            "allowed and not denied -> granted"
        );
        assert!(probe_denied(&review(false, None)), "not allowed -> denied");
        assert!(
            probe_denied(&review(true, Some(true))),
            "explicitly denied -> denied"
        );
        // A response with no status at all fails closed.
        assert!(probe_denied(&SelfSubjectAccessReview::default()));
    }

    #[test]
    fn rbac_message_names_namespace_and_denied_verbs() {
        let denied = [
            Probe {
                group: GROUP_COORDINATION,
                resource: RESOURCE_LEASES,
                verb: "update",
            },
            Probe {
                group: GROUP_CACHE,
                resource: RESOURCE_CACHE,
                verb: "watch",
            },
        ];
        let msg = rbac_failure_message("gears", &denied);
        assert!(msg.contains("gears"));
        assert!(msg.contains("`update` `leases`"));
        assert!(msg.contains("`watch` `clustercacheentries`"));
        assert!(msg.contains("section 7"));
    }
}
