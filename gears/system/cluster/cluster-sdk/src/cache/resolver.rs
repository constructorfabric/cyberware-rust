//! The fluent cache resolver and its startup capability-validation helper.

use toolkit::client_hub::ClientHub;

use crate::binding;
use crate::cache::facade::ClusterCacheV1;
use crate::cache::types::{CacheCapability, CacheConsistency, CacheFeatures};
use crate::dto::CacheDescriptor;
use crate::error::ClusterError;
use crate::intern::intern;
use crate::profile::{ClusterProfile, validate_cluster_name};

/// A fluent builder that resolves a [`ClusterCacheV1`] for a profile and
/// validates declared capabilities at startup.
#[must_use = "a resolver builder resolves nothing until `.resolve()` is called"]
pub struct CacheResolverBuilder<'a> {
    hub: &'a ClientHub,
    profile_name: Option<&'static str>,
    requirements: Vec<CacheCapability>,
}

impl<'a> CacheResolverBuilder<'a> {
    pub(crate) fn new(hub: &'a ClientHub) -> Self {
        Self {
            hub,
            profile_name: None,
            requirements: Vec::new(),
        }
    }

    /// Binds the resolution to a typed profile. The marker is passed by type;
    /// only its [`ClusterProfile::NAME`] is read.
    pub fn profile<P: ClusterProfile>(mut self, _marker: P) -> Self {
        self.profile_name = Some(P::NAME);
        self
    }

    /// Declares a capability the bound backend must satisfy.
    pub fn require(mut self, capability: CacheCapability) -> Self {
        self.requirements.push(capability);
        self
    }

    /// Resolves the cache facade for the bound profile.
    ///
    /// # Why this is `async`
    ///
    /// A remote binding cannot validate a declared capability without a
    /// [`ProfileDescriptor`](crate::ProfileDescriptor), and fetching one is I/O
    /// (DESIGN.md, obstacle A). **This is the only SDK
    /// signature the deployable model changes** (invariant I2) — the facades, the
    /// typed-profile resolver, `scoped()`, the watch-event unions and
    /// `auto_restart` all keep their shapes.
    ///
    /// It costs a consumer nothing beyond an `.await`: facades are resolved in a
    /// gear's `start`, never its `init`, and both are already `async fn`.
    ///
    /// In Profile 1 the await is a formality — the local client's descriptor is
    /// intrinsic, so its future is ready on the first poll and the bounded timeout
    /// can never fire.
    ///
    /// # What it resolves through
    ///
    /// The process's single `dyn ClusterClient` (invariant I4), never a
    /// per-profile hub registration: the client is what a remote consumer has, and
    /// routing both profiles through it is what makes this one code path
    /// (§4.9.3). See the `binding` module for the four steps and for
    /// what happens when no client is registered — `Ok`, and a facade that reports
    /// [`ClusterError::ProfileNotBound`] on first use (§4.9.1).
    ///
    /// # Errors
    /// - [`ClusterError::ProfileNotSpecified`] if no profile was set.
    /// - [`ClusterError::InvalidName`] if the bound profile's
    ///   [`NAME`](ClusterProfile::NAME) violates [`CLUSTER_NAME_RULE`](crate::CLUSTER_NAME_RULE).
    /// - [`ClusterError::ProfileNotBound`] if a cluster client is registered but
    ///   binds no cache backend for the profile.
    /// - [`ClusterError::CapabilityNotMet`] if a declared capability is
    ///   unsupported by the bound backend, and the profile's descriptor was
    ///   obtainable within the SDK's bounded resolve timeout.
    ///   When it was not, validation is deferred to the readiness contributor and
    ///   this returns `Ok` (§4.7.1).
    pub async fn resolve(self) -> Result<ClusterCacheV1, ClusterError> {
        let profile = self.profile_name.ok_or(ClusterError::ProfileNotSpecified)?;
        validate_cluster_name(profile)?;
        let requirements = self.requirements;
        let backend = binding::bind(
            self.hub,
            profile,
            "cache",
            |client| client.cache_backend(profile),
            || binding::unbound_cache(profile),
            move |descriptor| validate_cache_capabilities_from(&descriptor.cache, &requirements),
        )
        .await?;
        Ok(ClusterCacheV1::from_backend(backend))
    }
}

/// Validates declared cache capabilities against the profile's **descriptor** —
/// what the server-side binding declares (DESIGN.md).
///
/// This is the form the resolve path uses in both deployment profiles, so the
/// diagnostic is byte-identical across them.
///
/// # Errors
/// Returns [`ClusterError::CapabilityNotMet`] — naming the primitive, the unmet
/// capability, and the **operator-facing** provider name — for the first
/// unsatisfied requirement.
pub fn validate_cache_capabilities_from(
    descriptor: &CacheDescriptor,
    reqs: &[CacheCapability],
) -> Result<(), ClusterError> {
    // Decode the wire mirror once, up front, so every watch-family arm reads the
    // same normalized view (`From<WireCacheFeatures>` applies the absent-is-
    // supported default and the `watch ⇒ prefix_watch` invariant). Reading the
    // raw wire bool in one arm and the decoded value in another would let a
    // skewed descriptor satisfy `PrefixWatch` yet fail `Watch`.
    let features = CacheFeatures::from(descriptor.features);
    // Matched exhaustively (no catch-all): although `CacheCapability` is
    // `#[non_exhaustive]`, within this crate every variant must be handled, so
    // adding a future capability fails to compile here rather than being
    // silently treated as satisfied.
    for cap in reqs {
        match cap {
            CacheCapability::Linearizable => {
                if CacheConsistency::from(descriptor.consistency) != CacheConsistency::Linearizable
                {
                    return Err(ClusterError::CapabilityNotMet {
                        primitive: "ClusterCacheV1",
                        capability: "Linearizable",
                        // Interned rather than the error widened to `String`: the
                        // frozen model keeps `&'static str` (invariant I3), and
                        // provider names are a bounded, config-derived set.
                        provider: intern(&descriptor.provider),
                    });
                }
            }
            CacheCapability::Watch => {
                // Reads the decoded `watch` bit (see `features` above) so a
                // consumer that requires a reactive feed is refused here rather
                // than at first `watch()` call.
                if !features.watch {
                    return Err(ClusterError::CapabilityNotMet {
                        primitive: "ClusterCacheV1",
                        capability: "Watch",
                        provider: intern(&descriptor.provider),
                    });
                }
            }
            CacheCapability::PrefixWatch => {
                if !features.prefix_watch {
                    return Err(ClusterError::CapabilityNotMet {
                        primitive: "ClusterCacheV1",
                        capability: "PrefixWatch",
                        provider: intern(&descriptor.provider),
                    });
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "resolver_tests.rs"]
mod resolver_tests;
