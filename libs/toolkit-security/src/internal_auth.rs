//! Platform-plane (workload) authentication primitives.
//!
//! The platform plane authenticates *which gear* is making a system-initiated
//! call that carries **no user context** (`DirectoryService` registration,
//! heartbeats, global GTS registration). It is deliberately separate from the
//! tenant plane (`Authorization: Bearer <jwt>` + [`SecurityContext`]): platform
//! calls carry an [`InternalCredential`] over the `X-ToolKit-Internal-Token`
//! header (never `Authorization`, to avoid colliding with the user JWT) and
//! resolve to a [`PlatformSecurityContext`] — a type **distinct** from the
//! tenant [`SecurityContext`] that is never evaluated by the tenant
//! `PolicyEnforcer`.
//!
//! This module owns only the transport-agnostic, dependency-light pieces: the
//! credential/identity types, a neutral error, and the [`InternalAuthenticator`]
//! authentication trait. The Axum middleware, the tonic interceptor, and the
//! concrete K8s `TokenReview` validator live in the transport / bootstrap
//! layers so this foundational crate stays free of `axum`, `tonic`, and `kube`.
//!
//! [`SecurityContext`]: crate::context::SecurityContext

use std::future::Future;
use std::path::PathBuf;

use secrecy::SecretString;

/// The credential a gear attaches to its system (platform-plane) calls.
///
/// Selected by deployment profile at the bootstrap layer. The variant set is
/// **frozen now** to keep the API stable across phases, but only
/// [`InternalCredential::None`] (Profile 1) and
/// [`InternalCredential::KubeServiceAccountToken`] (Profile 3) are wired in the
/// first phase. [`InternalCredential::BootstrapToken`] (Profile 2) and
/// [`InternalCredential::MtlsIdentity`] (mTLS end state) are struct-only —
/// their validation/wiring is deferred to a later phase.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum InternalCredential {
    /// Profile 1 (in-process): no credential — the process boundary is the
    /// trust root, so no header/metadata is attached.
    None,
    /// Profile 2 (single-node): an ephemeral bootstrap token minted by the
    /// Platform Host. Struct-only in the first phase; validation deferred to P2.
    BootstrapToken(SecretString),
    /// Profile 3 (K8s): a projected `ServiceAccount` JWT (auto-mounted,
    /// auto-rotated). `token_path` is the projected-volume path; `audience` is
    /// the expected token audience (e.g. `toolkit-internal`).
    KubeServiceAccountToken {
        /// Path to the projected SA token file.
        token_path: PathBuf,
        /// Expected audience the token must be scoped to.
        audience: String,
    },
    /// End state (and Profile 2 multi-node): an mTLS client identity. Struct-only
    /// here; mTLS validation/wiring is deferred to a later phase.
    MtlsIdentity {
        /// Client certificate path.
        cert: PathBuf,
        /// Client private-key path.
        key: PathBuf,
        /// Trust-anchor (CA bundle) path.
        ca: PathBuf,
    },
}

/// Method-agnostic platform identity produced by validating an
/// [`InternalCredential`].
///
/// The variant reflects *which credential* authenticated the caller; platform
/// handlers consume a [`PlatformSecurityContext`] without branching on it. New
/// authentication methods add a variant — hence `#[non_exhaustive]` — without
/// changing [`PlatformSecurityContext`]'s shape.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
#[serde(tag = "type")]
pub enum PlatformIdentity {
    /// First phase: a validated K8s `ServiceAccount` (from a projected SA token
    /// verified via the `TokenReview` API).
    KubernetesServiceAccount {
        /// Namespace the `ServiceAccount` belongs to.
        namespace: String,
        /// `ServiceAccount` name.
        service_account: String,
        /// Originating pod name, when the token review reports it.
        pod: Option<String>,
    },
    /// Next phase: an mTLS + SPIFFE workload identity parsed from the X.509 SAN
    /// (`spiffe://<trust_domain>/gear/<name>/<version>`). Reserved — not
    /// populated in the first phase.
    Spiffe {
        /// SPIFFE trust domain.
        trust_domain: String,
        /// Workload name component of the SPIFFE ID (the gear segment of
        /// `spiffe://<trust_domain>/gear/<name>/<version>`).
        name: String,
        /// Version component of the SPIFFE ID.
        version: String,
    },
    /// A caller authenticated by a pre-shared secret (dev / single-node
    /// profiles). Not tied to any deployment substrate: it exists so the
    /// platform plane can be exercised end-to-end without Kubernetes. The
    /// `name` is a configured label used for workload-policy decisions.
    Shared {
        /// Configured caller label (returned by [`PlatformIdentity::peer_name`]).
        name: String,
    },
    /// Catch-all for variants introduced in a newer library version.
    ///
    /// Produced only by `serde::Deserialize` when the `"type"` field holds an
    /// unrecognised value. Never constructed directly; `peer_name` returns
    /// `"<unknown>"` for this variant.
    #[serde(other)]
    Unknown,
}

impl PlatformIdentity {
    /// The caller's name, distilled for workload-policy decisions.
    ///
    /// For a [`PlatformIdentity::KubernetesServiceAccount`] this is the `ServiceAccount`
    /// name; for [`PlatformIdentity::Spiffe`] it is the workload (gear) component
    /// of the SPIFFE ID.
    #[must_use]
    pub fn peer_name(&self) -> &str {
        match self {
            Self::KubernetesServiceAccount {
                service_account, ..
            } => service_account,
            Self::Spiffe { name, .. } | Self::Shared { name } => name,
            Self::Unknown => "<unknown>",
        }
    }
}

/// Identity for non-tenant, platform-scoped operations.
///
/// **Never** carries a tenant subject and is **never** passed to the tenant
/// `PolicyEnforcer`. This is a separate type from the tenant
/// [`SecurityContext`] by design (separation of mechanisms): "this call has no
/// tenant" is unrepresentable-as-a-tenant rather than encoded as a nil tenant
/// id. The wrapper is stable across authentication phases; only its
/// [`PlatformIdentity`] changes.
///
/// [`SecurityContext`]: crate::context::SecurityContext
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PlatformSecurityContext {
    identity: PlatformIdentity,
}

impl PlatformSecurityContext {
    /// Wrap a validated [`PlatformIdentity`].
    #[must_use]
    pub fn new(identity: PlatformIdentity) -> Self {
        Self { identity }
    }

    /// Construct a credential-free **outbound plane marker**.
    ///
    /// This is **not** a validated identity. It exists so gear-layer code (e.g.
    /// a `PolicyEnforcer`) can call a platform-plane contract method whose
    /// signature takes a [`PlatformSecurityContext`] *argument* for compile-time
    /// plane selection, without possessing — or being able to forge — a real
    /// one. The marker carries no secret and is **discarded** by the generated
    /// client: the actual platform credential is attached below the contract
    /// layer from the process `InternalTokenProvider`, and the receiver
    /// re-derives the real identity by validating that wire token
    /// (`cpt-cf-adr-two-plane-auth`).
    ///
    /// It MUST never be consulted for an authorization decision — its
    /// [`PlatformIdentity`] is [`PlatformIdentity::Unknown`] precisely so a
    /// consumer that mistakenly reads it cannot derive a meaningful caller name.
    #[must_use]
    pub fn outbound_marker() -> Self {
        Self {
            identity: PlatformIdentity::Unknown,
        }
    }

    /// The validated platform identity backing this context.
    #[must_use]
    pub fn identity(&self) -> &PlatformIdentity {
        &self.identity
    }

    /// Consume the context, returning the owned [`PlatformIdentity`].
    #[must_use]
    pub fn into_identity(self) -> PlatformIdentity {
        self.identity
    }
}

/// Lightweight marker that *some* gear authenticated as a peer.
///
/// Distilled to the caller's name and consumed **only** by workload-policy
/// checks (e.g. "only `flight-control` may call `DeregisterInstance`"). It is
/// **not** a prerequisite for trusting a user context: the tenant JWT is
/// self-authenticating and is always re-validated regardless of peer trust.
/// It never substitutes for tenant-plane validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerAuthenticated {
    /// The authenticated caller's name.
    pub name: String,
}

/// Runtime signal that "this listener enforces platform-plane auth".
///
/// Stamped onto every request handled by an active platform-plane enforcement
/// layer — the gRPC `InternalAuthGrpcLayer` (before its exempt check, so even
/// exempt methods carry it) and the HTTP `internal_auth_middleware`. It lets a
/// handler distinguish two token-less cases that both lack a
/// [`PlatformSecurityContext`]:
///
/// - **Marker present, no context** — an enforcing listener let an *anonymous*
///   caller through (a permissive or exempt path). A handler that authorizes
///   per-peer must fail closed here: an unauthenticated caller must not be
///   treated as more privileged than an honest token holder.
/// - **Marker absent** — no platform-plane enforcement is installed (Profile 1
///   / in-process); there is no trust boundary to honour, so handlers fail open.
///
/// The marker carries no identity and grants nothing on its own; it only
/// reports the listener's posture, so a handler never has to read another
/// component's configuration to learn it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlatformAuthEnforced;

/// Neutral platform-plane authentication error.
///
/// Intentionally coarse-grained and transport-agnostic: it never carries the
/// token or provider-specific detail, so it is safe to surface at a trust
/// boundary. Concrete validators map their own errors into these variants.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum InternalAuthNError {
    /// The credential was present but failed validation (bad signature, wrong
    /// audience, expired, not authenticated by the backend, etc.).
    #[error("invalid internal credential")]
    InvalidToken,
    /// The validation backend (e.g. the K8s `TokenReview` API) could not be
    /// reached or returned a transient failure. Callers may retry or surface 503.
    #[error("internal-auth backend unavailable")]
    Unavailable,
    /// Any other validation failure. The message must not contain the token or
    /// other sensitive material.
    #[error("internal authentication failed: {0}")]
    Other(String),
}

/// Authenticates a raw platform-plane credential and resolves the caller's
/// [`PlatformIdentity`].
///
/// The transport layer (Axum middleware / tonic interceptor) stays generic over
/// this trait; the concrete validator (K8s `TokenReview` in the first phase) is
/// supplied at the gear/bootstrap layer so neither `toolkit-http` nor
/// `toolkit-transport-grpc` depend on `kube`.
///
/// The returned future is `Send` so the trait can be used from Axum/Tower
/// middleware on a multi-threaded runtime.
pub trait InternalAuthenticator: Send + Sync {
    /// Authenticate the raw `X-ToolKit-Internal-Token` value and resolve the
    /// caller's [`PlatformIdentity`].
    ///
    /// # Errors
    ///
    /// Returns [`InternalAuthNError`] if the credential is invalid, the backend
    /// is unavailable, or authentication otherwise fails.
    fn authenticate(
        &self,
        token: &str,
    ) -> impl Future<Output = Result<PlatformIdentity, InternalAuthNError>> + Send;
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn platform_identity_peer_name() {
        let sa = PlatformIdentity::KubernetesServiceAccount {
            namespace: "toolkit".to_owned(),
            service_account: "flight-control".to_owned(),
            pod: Some("flight-control-0".to_owned()),
        };
        assert_eq!(sa.peer_name(), "flight-control");

        let spiffe = PlatformIdentity::Spiffe {
            trust_domain: "example.org".to_owned(),
            name: "mini-chat".to_owned(),
            version: "1.0.0".to_owned(),
        };
        assert_eq!(spiffe.peer_name(), "mini-chat");
    }

    #[test]
    fn platform_security_context_wraps_identity() {
        let identity = PlatformIdentity::KubernetesServiceAccount {
            namespace: "toolkit".to_owned(),
            service_account: "directory-service".to_owned(),
            pod: None,
        };
        let ctx = PlatformSecurityContext::new(identity.clone());
        assert_eq!(ctx.identity(), &identity);
        assert_eq!(ctx.into_identity(), identity);
    }

    #[test]
    fn outbound_marker_carries_no_identity() {
        // The marker is a credential-free plane selector; its identity is
        // deliberately `Unknown` so a consumer that mistakenly reads it cannot
        // derive a meaningful caller name.
        let marker = PlatformSecurityContext::outbound_marker();
        assert_eq!(marker.identity(), &PlatformIdentity::Unknown);
        assert_eq!(marker.identity().peer_name(), "<unknown>");
    }

    #[test]
    fn platform_security_context_roundtrips_serde() {
        let ctx = PlatformSecurityContext::new(PlatformIdentity::KubernetesServiceAccount {
            namespace: "toolkit".to_owned(),
            service_account: "directory-service".to_owned(),
            pod: None,
        });
        let json = serde_json::to_string(&ctx).unwrap();
        let back: PlatformSecurityContext = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ctx);
    }
}
