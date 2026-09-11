//! Registration authorization — pure policy for the `DirectoryService`.
//!
//! Identity in, allow/deny out: no transport (`tonic`) types. The gRPC adapter
//! ([`crate::server`]) reads the authenticated peer off the request and calls
//! [`registration_authorized`] rather than owning the decision itself.

use std::collections::HashSet;

use toolkit_security::PlatformIdentity;

/// Registration-authorization policy for the `DirectoryService`.
///
/// All three sets are empty by default. `trusted_registrars` lists peers
/// allowed to act on *any* gear; `platform_namespaces` / `trust_domains` are the
/// Kubernetes namespaces / SPIFFE trust domains a per-gear identity must belong
/// to (empty disables the respective qualifier check — see
/// [`registration_authorized`]).
#[derive(Debug, Default, Clone)]
pub struct RegistrationPolicy {
    /// Peer names permitted to act on any gear's registration.
    pub trusted_registrars: HashSet<String>,
    /// Platform-controlled Kubernetes namespaces a `ServiceAccount` identity
    /// must live in to register.
    pub platform_namespaces: HashSet<String>,
    /// SPIFFE trust domains a workload identity must belong to.
    pub trust_domains: HashSet<String>,
}

/// Whether `identity` may register/deregister/heartbeat `gear_name`.
///
/// One predicate, correct across every platform-plane provider:
///
/// - [`PlatformIdentity::Shared`] (shared secret / bootstrap token): allowed.
///   A single, deliberately-shared trust boundary — per-gear binding is not
///   expressible, so this is honest rather than a silent bypass.
/// - Per-gear identity ([`PlatformIdentity::KubernetesServiceAccount`] with
///   SA-per-gear, [`PlatformIdentity::Spiffe`]): may act only on its own gear,
///   or on any gear when listed in `trusted_registrars`. The *unqualified* name
///   ([`PlatformIdentity::peer_name`]) is not enough on its own: the token
///   authenticator has no namespace/trust-domain allowlist, so a `billing`
///   `ServiceAccount` in *any* namespace (or a `billing` workload from *any*
///   SPIFFE trust domain) would otherwise be authorized for gear `billing`.
///   The identity's qualifier (K8s `namespace`, SPIFFE `trust_domain`) is
///   therefore checked against `platform_namespaces` / `trust_domains` first.
///   When the relevant allowlist is empty the qualifier check is skipped
///   (backward-compatible fallback), leaving today's name-only behavior.
/// - [`PlatformIdentity::Unknown`] (a future/unrecognised variant): fails
///   closed.
#[must_use]
pub fn registration_authorized(
    identity: &PlatformIdentity,
    gear_name: &str,
    policy: &RegistrationPolicy,
) -> bool {
    // A qualifier (K8s namespace / SPIFFE trust domain) is accepted when its
    // allowlist is unset (fallback) or explicitly lists it.
    let qualifier_ok = |allowlist: &HashSet<String>, value: &str| {
        allowlist.is_empty() || allowlist.contains(value)
    };
    // The name component may act on its own gear, or on any gear if a trusted
    // registrar.
    let name_ok = |name: &str| name == gear_name || policy.trusted_registrars.contains(name);

    match identity {
        PlatformIdentity::Shared { .. } => true,
        PlatformIdentity::KubernetesServiceAccount {
            namespace,
            service_account,
            ..
        } => qualifier_ok(&policy.platform_namespaces, namespace) && name_ok(service_account),
        PlatformIdentity::Spiffe {
            trust_domain, name, ..
        } => qualifier_ok(&policy.trust_domains, trust_domain) && name_ok(name),
        // `Unknown` and any future non_exhaustive variant fail closed.
        _ => false,
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    /// A [`RegistrationPolicy`] from `&str` slices.
    fn policy(trusted: &[&str], namespaces: &[&str], domains: &[&str]) -> RegistrationPolicy {
        let set = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect();
        RegistrationPolicy {
            trusted_registrars: set(trusted),
            platform_namespaces: set(namespaces),
            trust_domains: set(domains),
        }
    }

    /// A per-gear (`ServiceAccount`) platform identity named `name`.
    fn sa_identity(name: &str) -> PlatformIdentity {
        PlatformIdentity::KubernetesServiceAccount {
            namespace: "toolkit".to_owned(),
            service_account: name.to_owned(),
            pod: None,
        }
    }

    fn spiffe_identity(trust_domain: &str, name: &str) -> PlatformIdentity {
        PlatformIdentity::Spiffe {
            trust_domain: trust_domain.to_owned(),
            name: name.to_owned(),
            version: "1.0.0".to_owned(),
        }
    }

    /// The predicate over every platform-plane identity variant, in isolation,
    /// with no namespace / trust-domain allowlist (the name-only fallback).
    #[test]
    fn registration_authorized_covers_every_provider() {
        let open = policy(&[], &[], &[]);
        let trusted = policy(&["flight-control"], &[], &[]);

        // Per-gear identity: only its own gear, unless a trusted registrar.
        assert!(registration_authorized(
            &sa_identity("billing"),
            "billing",
            &open
        ));
        assert!(!registration_authorized(
            &sa_identity("billing"),
            "catalog",
            &open
        ));
        assert!(registration_authorized(
            &sa_identity("flight-control"),
            "billing",
            &trusted
        ));

        // SPIFFE workload name is the gear name -> strict with zero config.
        let spiffe = spiffe_identity("example.org", "billing");
        assert!(registration_authorized(&spiffe, "billing", &open));
        assert!(!registration_authorized(&spiffe, "catalog", &open));

        // Shared secret: one deliberately-shared trust boundary -> always allowed.
        let shared = PlatformIdentity::Shared {
            name: "toolkit-internal".to_owned(),
        };
        assert!(registration_authorized(&shared, "anything", &open));

        // Unknown / future variant fails closed.
        assert!(!registration_authorized(
            &PlatformIdentity::Unknown,
            "billing",
            &open
        ));
    }

    /// With an allowlist configured, the identity's qualifier (K8s namespace /
    /// SPIFFE trust domain) must match — a same-named `ServiceAccount` in a
    /// foreign namespace (or workload in a foreign trust domain) is rejected
    /// even though the name matches the gear.
    #[test]
    fn registration_authorized_enforces_qualifier_when_configured() {
        let ns = policy(&[], &["platform"], &[]);
        let td = policy(&[], &[], &["platform.example"]);

        let sa = |namespace: &str, gear: &str| PlatformIdentity::KubernetesServiceAccount {
            namespace: namespace.to_owned(),
            service_account: gear.to_owned(),
            pod: None,
        };

        // Right name in the platform namespace -> allowed; foreign namespace ->
        // rejected (the vuln this closes).
        assert!(registration_authorized(
            &sa("platform", "billing"),
            "billing",
            &ns
        ));
        assert!(!registration_authorized(
            &sa("tenant-x", "billing"),
            "billing",
            &ns
        ));

        // SPIFFE: same rule on the trust domain.
        assert!(registration_authorized(
            &spiffe_identity("platform.example", "billing"),
            "billing",
            &td,
        ));
        assert!(!registration_authorized(
            &spiffe_identity("evil.example", "billing"),
            "billing",
            &td,
        ));
    }

    /// A trusted registrar is still bound by the qualifier allowlist: the rule
    /// is `qualifier_ok && name_ok`, so "trusted" grants *cross-gear* authority,
    /// not a bypass of the namespace / trust-domain check. Guards against a
    /// regression to "trusted means skip the qualifier", which would let an SA
    /// named `flight-control` in *any* namespace own every gear.
    #[test]
    fn trusted_registrar_is_still_bound_by_the_qualifier_allowlist() {
        let sa = |namespace: &str, gear: &str| PlatformIdentity::KubernetesServiceAccount {
            namespace: namespace.to_owned(),
            service_account: gear.to_owned(),
            pod: None,
        };

        // Both sets populated: `flight-control` may act on any gear, but only
        // from the `platform` namespace / `platform.example` trust domain.
        let pol = policy(&["flight-control"], &["platform"], &["platform.example"]);

        // Trusted registrar in an allowed qualifier, acting cross-gear -> allowed.
        assert!(registration_authorized(
            &sa("platform", "flight-control"),
            "billing",
            &pol
        ));
        assert!(registration_authorized(
            &spiffe_identity("platform.example", "flight-control"),
            "billing",
            &pol
        ));

        // Same trusted registrar from a foreign qualifier -> denied. Trusted
        // does not skip the namespace / trust-domain check.
        assert!(!registration_authorized(
            &sa("tenant-x", "flight-control"),
            "billing",
            &pol
        ));
        assert!(!registration_authorized(
            &spiffe_identity("evil.example", "flight-control"),
            "billing",
            &pol
        ));
    }
}
