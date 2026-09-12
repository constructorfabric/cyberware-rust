//! What a facade binds to, and the steps that bind it
//! (DESIGN.md).
//!
//! Every resolver runs the same four steps, so they live here once rather than
//! three times:
//!
//! 1. **Take the process's `dyn ClusterClient` from the hub.** There is exactly
//!    one (invariant I4), and which implementation it is decides nothing here —
//!    that was settled by whoever registered it (§4.9.1).
//! 2. **Ask it for this profile's backend.** Sync and pure in both profiles: the
//!    real backend `Arc` locally, a remote handle remotely. No I/O.
//! 3. **Await this profile's descriptor, bounded.** The only `await` on the
//!    resolve path, and the reason `resolve()` is `async` at all (§4.7.1,
//!    obstacle A). It waits on the descriptor, never on cluster becoming
//!    reachable (invariant I6).
//! 4. **Validate the declared requirements against it** — inline when the
//!    descriptor landed, deferred to the readiness contributor when it did not.
//!
//! # There is no fallback branch
//!
//! The steps are the same code in both deployment profiles, and no step asks
//! which profile it is running in. A profile cannot resolve *toward* anything
//! other than what was registered, so the split-brain hazard a mode flag would
//! have to guard against is structurally absent rather than merely checked
//! (§4.9.3).
//!
//! # Validation reads the descriptor, not the backend
//!
//! Today's resolvers validate against the bound backend's own `consistency()` /
//! `features()` / `provider_name()`. A remote handle cannot answer those without
//! a descriptor, so the descriptor becomes the single source for **both**
//! profiles rather than one path reading the backend and the other reading the
//! wire. Locally the descriptor is computed from the real backends, so the answer
//! is identical — and the error text is now byte-identical across profiles, which
//! is what the inline-validation parity gate asserts (test gate "Inline
//! validation").
//!
//! One thing does change on the config-driven path: `CapabilityNotMet.provider`
//! becomes the provider name the **operator wrote** (`postgres`) rather than the
//! concrete Rust type behind it. That is what §5.5 asks for — an operator reading
//! a capability failure has to see which real backend failed the requirement.
//!
//! # An empty hub yields an unbound backend, not an error
//!
//! When no client is registered, `resolve()` returns `Ok` and the facade is built
//! over an [`UnboundCacheBackend`] (or its lock / leader-election sibling): a
//! backend whose every operation is
//! [`ClusterError::ProfileNotBound`](crate::ClusterError::ProfileNotBound) and
//! whose synchronous accessors answer with the weakest reading, so a declared
//! requirement can only fail rather than be falsely satisfied. That is §4.9.1's
//! rule — absence is tolerated by `resolve()`, named by the first call, and
//! reported by readiness — with no new error variant (invariant I3) and no branch
//! on the call path: the facade holds one `Arc<dyn _Backend>` either way, exactly
//! as it does today.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use toolkit::client_hub::ClientHub;

use crate::cache::{
    CacheConsistency, CacheEntry, CacheFeatures, CacheWatch, ClusterCacheBackend, PutRequest, Ttl,
};
use crate::client::ClusterClient;
use crate::dto::ProfileDescriptor;
use crate::error::ClusterError;
use crate::leader::{LeaderElectionBackend, LeaderElectionFeatures, LeaderWatch};
use crate::lock::{DistributedLockBackend, LockFeatures, LockGuard};
use crate::requirements::requirements;

/// How long [`bind`] waits for a profile's descriptor before deferring
/// validation to readiness (§4.7.1).
///
/// An SDK constant, deliberately not consumer-facing config (invariant I9): it is
/// a startup-path cost sized "in the hundreds of milliseconds to low seconds" —
/// long enough that a reachable cluster always answers, short enough not to stall
/// the framework's start phase behind an absent one.
///
/// It never fires in Profile 1: the local client's descriptor is intrinsic and
/// its future is ready on first poll, which `LocalClusterClient`'s own test
/// asserts rather than assumes.
pub const RESOLVE_DESCRIPTOR_TIMEOUT: Duration = Duration::from_secs(2);

/// What a first call against an unbound facade logs, and the phrase §4.9.1 uses
/// to distinguish "nothing is wired in this process" from "the server does not
/// bind that profile".
///
/// It is a **log** line rather than the error's message because
/// [`ClusterError::ProfileNotBound`]'s `Display` is part of the frozen error
/// model (invariant I3): varying the message would mean a second field or a
/// second variant, and the criterion that forbids the variant is the same one
/// that forbids the field.
pub const NOTHING_WIRED: &str = "no cluster client registered in this process";

/// The process's cluster client, if one is registered — self-constructing one
/// when the hub is empty and this process can build a remote (§4.7.1, §4.9.3
/// step 1).
///
/// `try_get` rather than `try_get_local`: a remote proxy registered by the
/// consumer wiring is a perfectly good client here, and local-wins is settled at
/// *registration* time (§4.9.3 step 1), not at resolve time.
///
/// # The self-construction arm, and what it is for
///
/// With `grpc-client` on and nothing in the hub, this builds and registers a
/// `RemoteClusterClient` rather than reporting absence. It is **defence, not the
/// mechanism**: the framework's proxy-wiring phase runs before `start` in both
/// deployment profiles, so the registration has normally already happened by the
/// time any consumer resolves. What this covers is a process where the phase did
/// not run at all — a consumer resolving outside a host runtime, a fixture, or a
/// wiring phase that failed — and it is the one place `resolve()` has a
/// feature-gated branch.
///
/// It cannot promote a *remote* client over a local one: the register step yields
/// to anything already in the hub, and a co-located cluster gear registers its
/// local client in `init`, before any consumer's `start` can reach this.
///
/// A derivation failure is logged and treated as absence rather than propagated.
/// The alternative would be `resolve()` returning `Err` on a *missing environment
/// variable*, which is neither of the two permanent errors §4.7 classifies and
/// would make an unconfigured deployment fail at a call site instead of at
/// readiness. The facade becomes unbound, the first call says
/// [`ClusterError::ProfileNotBound`], and the log says why.
pub fn process_client(hub: &ClientHub) -> Option<Arc<dyn ClusterClient>> {
    if let Some(client) = hub.try_get::<dyn ClusterClient>() {
        return Some(client);
    }
    #[cfg(feature = "grpc-client")]
    {
        // `None`: the credential arrives through the runtime's `WireFn`, and this
        // arm runs precisely when that phase did not, so there is none to pass. A
        // client built here attaches no `X-ToolKit-Internal-Token` and an
        // enforcing server will reject it.
        match crate::wiring::register_remote_client(hub, None) {
            Ok(client) => {
                tracing::info!(
                    "cluster resolve: no client was wired in this process, so one was built \
                     on demand (the proxy-wiring phase did not run)"
                );
                return Some(client);
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "cluster resolve: {NOTHING_WIRED} and a remote client could not be derived"
                );
            }
        }
    }
    None
}

/// Runs §4.9.3's four steps and returns the backend the facade wraps.
///
/// Generic over the backend trait so all three primitives share one
/// implementation of the ordering, the timeout and the permanent-error rule.
///
/// # Errors
/// - [`ClusterError::ProfileNotBound`] when a client is registered but does not
///   bind this profile — the loud, immediate failure §4.7 classifies as a
///   permanent config error.
/// - [`ClusterError::CapabilityNotMet`] when the descriptor landed and a declared
///   requirement is unmet.
/// - Any permanent error the descriptor fetch itself reports.
pub async fn bind<B: ?Sized>(
    hub: &ClientHub,
    profile: &'static str,
    primitive: &'static str,
    factory: impl FnOnce(&dyn ClusterClient) -> Result<Arc<B>, ClusterError>,
    unbound: impl FnOnce() -> Arc<B>,
    validate: impl Fn(&ProfileDescriptor) -> Result<(), ClusterError> + Send + Sync + 'static,
) -> Result<Arc<B>, ClusterError> {
    // 1. The client, not the backend.
    let client = process_client(hub);

    // `K5`'s two recording hooks, and they run on **every** resolve including the
    // ones that fail below. That is the point: the requirement registry is what
    // enforces invariant I5 when this resolve cannot (§4.7.1), so it has to learn
    // about the resolve before anything here can return.
    //
    // `set_client_seen` is what makes the nothing-wired verdict possible at all --
    // absence is tolerated here and reported there.
    //
    // The validator is shared rather than moved, so the inline check below and the
    // recorded copy are one closure over one set of requirements. They are therefore
    // the same code producing the same diagnostic -- but they are **separate calls**,
    // which matters: routing the inline check through the registry instead would make
    // it read "the latest recording for this (profile, primitive)", and two consumers
    // resolving the same pair with different requirements would validate against each
    // other's. That is not hypothetical - it is what the first version of this did,
    // and `resolve_rejects_capability_mismatch_at_startup` caught it.
    let validate = Arc::new(validate);
    requirements().set_client_seen(client.is_some());
    requirements().record(profile, primitive, {
        let validate = Arc::clone(&validate);
        Box::new(move |descriptor| validate(descriptor))
    });
    // And arm the level trigger for the missing-`healthcheck()` warning. It lives
    // here rather than inside `record` for one reason: `record` takes `&self` and a
    // spawned task needs `'static`, and `requirements()` is the only `&'static`
    // registry there is. `bind` is `async`, so a runtime is guaranteed; the armer
    // is idempotent, so the second and fiftieth resolve cost a relaxed swap.
    requirements().arm_contributor_silence_warning();

    let Some(client) = client else {
        tracing::warn!(
            profile,
            primitive,
            "cluster resolve: {NOTHING_WIRED}; the facade will report ProfileNotBound on first \
             use, and readiness will report the misconfiguration"
        );
        return Ok(unbound());
    };

    // 2. Sync, pure, no I/O - the real backend locally, a handle remotely.
    let backend = factory(client.as_ref()).map_err(|err| name_profile(err, profile))?;

    // 3. Bounded descriptor await, and 4. validate inline when it lands. The
    //    recorded copy above is what re-checks it on §5.6's refresh and what stands
    //    in for it when the descriptor did not arrive.
    validate_when_available(client.as_ref(), profile, primitive, |descriptor| {
        validate(descriptor)
    })
    .await?;

    Ok(backend)
}

/// Steps 3 and 4: await the descriptor on a bounded timeout and validate against
/// it, or record that validation was deferred.
///
/// # Errors
/// The unmet requirement, or a permanent descriptor failure.
async fn validate_when_available(
    client: &dyn ClusterClient,
    profile: &'static str,
    primitive: &'static str,
    validate: impl FnOnce(&ProfileDescriptor) -> Result<(), ClusterError>,
) -> Result<(), ClusterError> {
    let deferred =
        match tokio::time::timeout(RESOLVE_DESCRIPTOR_TIMEOUT, client.descriptor(profile)).await {
            Ok(Ok(descriptor)) => {
                validate(&descriptor)?;
                tracing::info!(profile, primitive, "cluster resolve: validated inline");
                return Ok(());
            }
            // A permanent failure is the caller's answer, not something to retry into
            // readiness (DESIGN section 4.7's classification).
            Ok(Err(err)) if is_permanent(&err) => return Err(name_profile(err, profile)),
            Ok(Err(err)) => err.to_string(),
            Err(_elapsed) => {
                format!("descriptor did not arrive within {RESOLVE_DESCRIPTOR_TIMEOUT:?}")
            }
        };
    tracing::info!(
        profile,
        primitive,
        reason = %deferred,
        "cluster resolve: validation deferred to readiness"
    );
    Ok(())
}

/// Whether a descriptor failure is one that cannot resolve on its own, and so is
/// returned from `resolve()` rather than deferred (§4.7's classification table).
///
/// The permanent set is exactly the two rows classified so: a profile the server
/// does not bind, and a requirement it cannot satisfy. Everything else — a
/// connection loss, a timeout, a cluster mid-shutdown — is transient, and
/// deferring it is what keeps a correct configuration from being flaky (§4.7.1).
fn is_permanent(err: &ClusterError) -> bool {
    matches!(
        err,
        ClusterError::ProfileNotBound { .. } | ClusterError::CapabilityNotMet { .. }
    )
}

/// Puts the **requested** profile name back onto a `ProfileNotBound` raised
/// below this point.
///
/// A client reports the profile it could not find, and it can only name one it
/// has seen: `ProfileRegistry` looks the name up with `intern_existing` and falls
/// back to a placeholder, precisely so a caller looping over made-up names cannot
/// grow the intern table (§5.2). The resolver is not in that position — its name
/// came from a [`ClusterProfile`](crate::ClusterProfile) marker and is already
/// `&'static` — so restoring it here keeps the diagnostic exact without interning
/// anything, and keeps the error text identical to what a pre-`K4` resolve
/// produced for the same misconfiguration.
///
/// Only the field is corrected; the variant is untouched, and every other error
/// passes through.
fn name_profile(err: ClusterError, profile: &'static str) -> ClusterError {
    match err {
        ClusterError::ProfileNotBound { .. } => ClusterError::ProfileNotBound { profile },
        other => other,
    }
}

/// The error every operation on an unbound backend reports.
fn not_bound(profile: &'static str) -> ClusterError {
    ClusterError::ProfileNotBound { profile }
}

/// The cache backend a facade holds when no cluster client was registered
/// (§4.9.1).
///
/// Every operation is `ProfileNotBound` naming the profile, which is the same
/// variant a reachable server returns for a profile it does not bind — the two
/// are distinguished by [`NOTHING_WIRED`] in the log, not in the error.
pub struct UnboundCacheBackend {
    profile: &'static str,
}

/// An unbound cache backend for `profile`, as the facade holds it.
///
/// A free function rather than a `new`, because what a caller wants here is the
/// erased `Arc<dyn _Backend>` a facade takes - the concrete type is not nameable
/// outside this crate anyway.
pub fn unbound_cache(profile: &'static str) -> Arc<dyn ClusterCacheBackend> {
    Arc::new(UnboundCacheBackend { profile })
}

#[async_trait]
impl ClusterCacheBackend for UnboundCacheBackend {
    /// The weakest reading, on the same rule the remote handles follow when their
    /// descriptor cache is empty: a consumer branching on this must find a
    /// declared requirement unsatisfied, never falsely satisfied.
    fn consistency(&self) -> CacheConsistency {
        CacheConsistency::EventuallyConsistent
    }

    fn features(&self) -> CacheFeatures {
        // An unbound stub can serve nothing, watch included: the weakest honest
        // reading, matching `consistency()` above.
        CacheFeatures::without_watch()
    }

    fn provider_name(&self) -> &'static str {
        "unbound"
    }

    async fn get(&self, _key: &str) -> Result<Option<CacheEntry>, ClusterError> {
        Err(not_bound(self.profile))
    }

    async fn put(&self, _req: PutRequest<'_>) -> Result<(), ClusterError> {
        Err(not_bound(self.profile))
    }

    async fn delete(&self, _key: &str) -> Result<bool, ClusterError> {
        Err(not_bound(self.profile))
    }

    async fn contains(&self, _key: &str) -> Result<bool, ClusterError> {
        Err(not_bound(self.profile))
    }

    async fn put_if_absent(
        &self,
        _req: PutRequest<'_>,
    ) -> Result<Option<CacheEntry>, ClusterError> {
        Err(not_bound(self.profile))
    }

    async fn compare_and_swap(
        &self,
        _key: &str,
        _expected_version: u64,
        _new_value: &[u8],
        _ttl: Ttl,
    ) -> Result<CacheEntry, ClusterError> {
        Err(not_bound(self.profile))
    }

    async fn compare_and_delete(
        &self,
        _key: &str,
        _expected_value: &[u8],
    ) -> Result<bool, ClusterError> {
        Err(not_bound(self.profile))
    }

    async fn watch(&self, _key: &str) -> Result<CacheWatch, ClusterError> {
        Err(not_bound(self.profile))
    }

    async fn watch_prefix(&self, _prefix: &str) -> Result<CacheWatch, ClusterError> {
        Err(not_bound(self.profile))
    }

    /// Overridden rather than left to the trait default, which reports
    /// `Unsupported { feature: "scan_prefix" }` — a claim about a *capability*,
    /// which is not what is wrong here.
    async fn scan_prefix(&self, _prefix: &str) -> Result<Vec<String>, ClusterError> {
        Err(not_bound(self.profile))
    }

    /// Reports the profile as not serving. Nothing in the gear probes this — the
    /// composite readiness check probes the profiles it wired, never a consumer's
    /// facade — but a defaulted `Ok(())` would be a lie if anything ever did.
    async fn probe(&self) -> Result<(), ClusterError> {
        Err(not_bound(self.profile))
    }
}

/// The distributed-lock backend a facade holds when no cluster client was
/// registered. See [`UnboundCacheBackend`].
pub struct UnboundLockBackend {
    profile: &'static str,
}

/// An unbound lock backend for `profile`, as the facade holds it.
///
/// A free function rather than a `new`, because what a caller wants here is the
/// erased `Arc<dyn _Backend>` a facade takes - the concrete type is not nameable
/// outside this crate anyway.
pub fn unbound_lock(profile: &'static str) -> Arc<dyn DistributedLockBackend> {
    Arc::new(UnboundLockBackend { profile })
}

#[async_trait]
impl DistributedLockBackend for UnboundLockBackend {
    fn features(&self) -> LockFeatures {
        LockFeatures::new(false)
    }

    fn provider_name(&self) -> &'static str {
        "unbound"
    }

    async fn try_lock(&self, _name: &str, _ttl: Duration) -> Result<LockGuard, ClusterError> {
        Err(not_bound(self.profile))
    }

    async fn lock(
        &self,
        _name: &str,
        _ttl: Duration,
        _timeout: Duration,
    ) -> Result<LockGuard, ClusterError> {
        Err(not_bound(self.profile))
    }

    /// The two acquisition methods a facade does not reach but a wrapper might.
    /// The token-predicated methods (`renew`, `release`) are deliberately left at
    /// their trait defaults: a token can only come from an acquisition against
    /// *this* backend, and no acquisition here ever succeeds.
    async fn acquire(
        &self,
        _name: &str,
        _owner: &str,
        _ttl: Duration,
    ) -> Result<crate::lease::LeaseToken, ClusterError> {
        Err(not_bound(self.profile))
    }

    async fn acquire_waiting(
        &self,
        _name: &str,
        _owner: &str,
        _ttl: Duration,
        _timeout: Duration,
    ) -> Result<crate::lease::LeaseToken, ClusterError> {
        Err(not_bound(self.profile))
    }

    async fn probe(&self) -> Result<(), ClusterError> {
        Err(not_bound(self.profile))
    }
}

/// The leader-election backend a facade holds when no cluster client was
/// registered. See [`UnboundCacheBackend`].
pub struct UnboundLeaderElectionBackend {
    profile: &'static str,
}

/// An unbound leader-election backend for `profile`, as the facade holds it.
///
/// A free function rather than a `new`, because what a caller wants here is the
/// erased `Arc<dyn _Backend>` a facade takes - the concrete type is not nameable
/// outside this crate anyway.
pub fn unbound_leader_election(profile: &'static str) -> Arc<dyn LeaderElectionBackend> {
    Arc::new(UnboundLeaderElectionBackend { profile })
}

#[async_trait]
impl LeaderElectionBackend for UnboundLeaderElectionBackend {
    fn features(&self) -> LeaderElectionFeatures {
        LeaderElectionFeatures::new(false)
    }

    fn provider_name(&self) -> &'static str {
        "unbound"
    }

    async fn elect(&self, _name: &str) -> Result<LeaderWatch, ClusterError> {
        Err(not_bound(self.profile))
    }

    async fn elect_with_config(
        &self,
        _name: &str,
        _config: crate::leader::ElectionConfig,
    ) -> Result<LeaderWatch, ClusterError> {
        Err(not_bound(self.profile))
    }

    /// As on the lock: `renew` and `resign` are token-predicated and stay
    /// defaulted, because no `join` here ever hands one out.
    async fn join(
        &self,
        _name: &str,
        _owner: &str,
        _config: crate::leader::ElectionConfig,
    ) -> Result<Option<crate::lease::LeaseToken>, ClusterError> {
        Err(not_bound(self.profile))
    }

    async fn probe(&self) -> Result<(), ClusterError> {
        Err(not_bound(self.profile))
    }
}

#[cfg(test)]
#[path = "binding_tests.rs"]
mod binding_tests;
