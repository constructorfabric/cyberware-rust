//! `kube::Client` construction/adoption and namespace/identity resolution
//! (DESIGN.md §3.3, §3.6).
//!
//! Two values are resolved once at `build_and_start` and pinned for the process's
//! lifetime: the **namespace** every object lives in, and this instance's
//! **identity** (written raw as `holderIdentity`). Both follow a fixed source
//! order so an operator never has to guess which source won, and both resolved
//! values land on the startup log line.
//!
//! The ordered resolution is factored into the two pure functions
//! [`resolve_namespace`] and [`resolve_identity`] — each takes the candidate
//! sources as already-read `Option`s, so the *ordering* logic is exhaustively
//! unit-testable without touching the environment, a file, or a kubeconfig. The
//! async wrappers ([`discover_namespace`], [`discover_identity`]) do only the I/O
//! of reading each source and delegate the decision to them.

use std::time::Duration;

use cluster_sdk::ClusterError;
use kube::{Client, Config};
use tracing::warn;

use crate::k8s_error::map_kube_error;

/// Env var carrying the pod's namespace (the downward API convention, §3.6).
const POD_NAMESPACE_ENV: &str = "POD_NAMESPACE";
/// Env var carrying the pod's name (the downward API convention, §3.6).
const POD_NAME_ENV: &str = "POD_NAME";
/// The projected service-account namespace file, present in every in-cluster pod.
const SA_NAMESPACE_PATH: &str = "/var/run/secrets/kubernetes.io/serviceaccount/namespace";
/// `holderIdentity` is a Lease *spec* value, not an object name, so RFC 1123 does
/// not apply; it is only truncated so an exotic source cannot produce an
/// unbounded string (§3.6).
const MAX_IDENTITY_LEN: usize = 512;

/// A fully resolved connection: the client plus the pinned namespace and identity.
pub struct ResolvedClient {
    /// The Kubernetes client — an HTTPS connection pool with a resolved auth
    /// strategy (§3.3). One per plugin handle.
    pub client: Client,
    /// The namespace every object this instance manages lives in (§3.6).
    pub namespace: String,
    /// This instance's identity, written raw as `holderIdentity` (§2.4, §3.6).
    pub identity: String,
}

/// The ordered namespace sources (§3.6), each already read into an `Option`.
///
/// Resolution prefers, in order: operator config, the `POD_NAMESPACE` env var, the
/// projected service-account namespace file, then the current kubeconfig context's
/// namespace. There is deliberately **no** `default` fallback.
#[derive(Debug, Default)]
pub struct NamespaceSources {
    /// `config.namespace` (already `${VAR}`-expanded).
    pub config: Option<String>,
    /// The `POD_NAMESPACE` environment variable.
    pub pod_namespace_env: Option<String>,
    /// The contents of [`SA_NAMESPACE_PATH`].
    pub service_account_file: Option<String>,
    /// The current kubeconfig context's `namespace` field (only `Some` when the
    /// context sets it explicitly — never a synthesized `default`).
    pub kubeconfig_context: Option<String>,
}

/// The ordered identity sources (§3.6), each already read into an `Option`.
#[derive(Debug, Default)]
pub struct IdentitySources {
    /// `config.identity` (already `${VAR}`-expanded).
    pub config: Option<String>,
    /// The `POD_NAME` environment variable.
    pub pod_name_env: Option<String>,
    /// The host name.
    pub hostname: Option<String>,
}

/// Resolves the namespace from its ordered sources (§3.6).
///
/// Returns the first source that is present and non-blank, trimmed. There is no
/// `default` fallback: silently coordinating in `default` because the downward API
/// was not wired is a cross-tenant collision waiting to happen, so an unresolved
/// namespace is a hard configuration error.
///
/// # Errors
///
/// [`ClusterError::InvalidConfig`] when no source yields a non-blank namespace,
/// naming the four sources so an operator knows what to wire.
pub fn resolve_namespace(sources: &NamespaceSources) -> Result<String, ClusterError> {
    let candidates = [
        &sources.config,
        &sources.pod_namespace_env,
        &sources.service_account_file,
        &sources.kubeconfig_context,
    ];
    candidates
        .into_iter()
        .flatten()
        .map(|s| s.trim())
        .find(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| ClusterError::InvalidConfig {
            reason: "namespace could not be resolved: set `namespace` in the provider config, or \
                     the POD_NAMESPACE env var (downward API), or run in-cluster with a projected \
                     service-account token, or select a kubeconfig context with a namespace. There \
                     is no `default` fallback (DESIGN.md 3.6)"
                .to_owned(),
        })
}

/// Resolves the identity from its ordered sources (§3.6), truncating at
/// [`MAX_IDENTITY_LEN`] characters.
///
/// Returns the resolved identity and whether it was truncated (the caller emits a
/// WARN in that case). Unlike the namespace, identity has a safe fallback chain —
/// a pod's hostname *is* its pod name — so this only fails if every source is
/// blank, which does not happen for any real process.
///
/// # Errors
///
/// [`ClusterError::InvalidConfig`] when no source yields a non-blank identity.
pub fn resolve_identity(sources: &IdentitySources) -> Result<(String, bool), ClusterError> {
    let raw = [&sources.config, &sources.pod_name_env, &sources.hostname]
        .into_iter()
        .flatten()
        .map(|s| s.trim())
        .find(|s| !s.is_empty())
        .ok_or_else(|| ClusterError::InvalidConfig {
            reason:
                "identity could not be resolved: set `identity` in the provider config, or the \
                     POD_NAME env var, or the HOSTNAME env var (which a Kubernetes pod sets to its \
                     pod name) (DESIGN.md 3.6)"
                    .to_owned(),
        })?;

    // Truncate on a character boundary, not a byte one, so a multi-byte identity
    // cannot be split mid-codepoint.
    let truncated: String = raw.chars().take(MAX_IDENTITY_LEN).collect();
    let was_truncated = truncated.chars().count() < raw.chars().count();
    Ok((truncated, was_truncated))
}

/// Reads the namespace sources and resolves the namespace (§3.6).
///
/// # Errors
///
/// Propagates [`resolve_namespace`]'s [`ClusterError::InvalidConfig`].
pub async fn discover_namespace(config_namespace: Option<&str>) -> Result<String, ClusterError> {
    let sources = NamespaceSources {
        config: config_namespace.map(str::to_owned),
        pod_namespace_env: std::env::var(POD_NAMESPACE_ENV).ok(),
        // `read_to_string` failing (not in a pod, permission) is simply "this source
        // is absent" — the next source is tried, and a hard miss is the resolver's.
        service_account_file: tokio::fs::read_to_string(SA_NAMESPACE_PATH).await.ok(),
        kubeconfig_context: kubeconfig_context_namespace(),
    };
    resolve_namespace(&sources)
}

/// Reads the identity sources and resolves the identity (§3.6), emitting a WARN
/// once if the resolved value had to be truncated.
///
/// # Errors
///
/// Propagates [`resolve_identity`]'s [`ClusterError::InvalidConfig`].
pub fn discover_identity(config_identity: Option<&str>) -> Result<String, ClusterError> {
    let sources = IdentitySources {
        config: config_identity.map(str::to_owned),
        pod_name_env: std::env::var(POD_NAME_ENV).ok(),
        hostname: hostname(),
    };
    let (identity, was_truncated) = resolve_identity(&sources)?;
    if was_truncated {
        warn!(
            len = MAX_IDENTITY_LEN,
            "cluster.provider.identity_truncated: resolved identity exceeded {MAX_IDENTITY_LEN} \
             characters and was truncated; set an explicit `identity` if this is not intended"
        );
    }
    Ok(identity)
}

/// The current kubeconfig context's explicit `namespace`, if any.
///
/// Returns `None` when there is no kubeconfig, no current context, or the context
/// does not set a namespace — the last of which is the important case: kube's own
/// [`Config`] would synthesize `"default"` there, and §3.6 forbids that fallback,
/// so this reads the raw context field (which is `None` unless set explicitly)
/// rather than `Config::default_namespace`.
fn kubeconfig_context_namespace() -> Option<String> {
    let kubeconfig = kube::config::Kubeconfig::read().ok()?;
    let current = kubeconfig.current_context.as_ref()?;
    kubeconfig
        .contexts
        .iter()
        .find(|c| &c.name == current)
        .and_then(|c| c.context.as_ref())
        .and_then(|ctx| ctx.namespace.clone())
}

/// The `HOSTNAME` environment variable, or `None` when it is unset, blank, or not
/// valid UTF-8. Container runtimes (including the kubelet) set it to the pod name;
/// a bare process outside a container often has none. Reads only the env var — no OS
/// hostname syscall — so the identity error text below names the same source.
fn hostname() -> Option<String> {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.trim().is_empty())
}

/// Builds a client from the inferred environment and resolves namespace/identity
/// (§3.3, §3.6) — the wiring path, where a provider receives only a serde options
/// map and has no pre-existing client to adopt.
///
/// `Config::infer()` loads a kubeconfig (from `KUBECONFIG` / `~/.kube/config`) or,
/// failing that, the in-cluster config. [`apply_timeouts`] then bounds connection
/// setup and writes on the client itself; per-call read budgets are applied by the
/// backends via `tokio::time::timeout` (§3.3), *not* a client-wide `read_timeout`,
/// which would sever the long-lived watch streams.
///
/// # Errors
///
/// [`ClusterError::InvalidConfig`] when the environment yields no usable config (a
/// malformed kubeconfig, an unresolvable in-cluster config), or when
/// namespace/identity cannot be resolved; [`ClusterError::Provider`] when the
/// client itself cannot be constructed from an otherwise-valid config.
pub async fn build(
    config_namespace: Option<&str>,
    config_identity: Option<&str>,
    request_timeout_ms: u64,
) -> Result<ResolvedClient, ClusterError> {
    let mut kube_config = Config::infer()
        .await
        .map_err(|err| ClusterError::InvalidConfig {
            reason: format!("could not infer a Kubernetes client configuration: {err}"),
        })?;
    apply_timeouts(&mut kube_config, request_timeout_ms);
    let client = Client::try_from(kube_config).map_err(|err| map_kube_error(&err))?;
    finish(client, config_namespace, config_identity).await
}

/// Adopts a caller-provided client and resolves namespace/identity (§3.3).
///
/// `with_client` exists because a gear that already holds a `kube::Client`
/// (mini-chat, chat-engine) should not authenticate twice. The wiring path does
/// not use it — a provider receives only a serde options map — so the adopted
/// client keeps whatever timeouts its owner configured; this path does not
/// re-apply [`apply_timeouts`].
///
/// # Errors
///
/// [`ClusterError::InvalidConfig`] when namespace/identity cannot be resolved.
// The `with_client` adoption path (DESIGN.md §3.3): a host gear that already holds
// a `kube::Client` (mini-chat, chat-engine) hands it in rather than authenticating
// twice. The wiring path uses `build`; the builders' `with_client` and the L3 test
// harness use this.
pub async fn adopt(
    client: Client,
    config_namespace: Option<&str>,
    config_identity: Option<&str>,
) -> Result<ResolvedClient, ClusterError> {
    finish(client, config_namespace, config_identity).await
}

/// Shared tail of [`build`] and [`adopt`]: resolve the namespace and identity for
/// an already-constructed `client`.
async fn finish(
    client: Client,
    config_namespace: Option<&str>,
    config_identity: Option<&str>,
) -> Result<ResolvedClient, ClusterError> {
    let namespace = discover_namespace(config_namespace).await?;
    let identity = discover_identity(config_identity)?;
    Ok(ResolvedClient {
        client,
        namespace,
        identity,
    })
}

/// Bounds connection setup and writes on the client, leaving reads at kube's
/// generous default (§3.3).
///
/// `connect_timeout` and `write_timeout` are set to the per-request budget: both
/// bound short, non-streaming operations, so a hung API server surfaces as a
/// failed call on schedule. `read_timeout` is **left untouched** on purpose — it
/// is an idle-read timeout on the connection, and a watch stream is idle between
/// events, so shortening it to the request budget would tear every long-lived
/// watch (§3.3, §4.3, §6.3) down on the first quiet interval. Per-call read
/// budgets are instead applied at each `get`/`list` site with
/// `tokio::time::timeout`.
fn apply_timeouts(config: &mut Config, request_timeout_ms: u64) {
    let budget = Duration::from_millis(request_timeout_ms);
    config.connect_timeout = Some(budget);
    config.write_timeout = Some(budget);
}

#[cfg(test)]
mod tests {
    use super::{
        IdentitySources, MAX_IDENTITY_LEN, NamespaceSources, resolve_identity, resolve_namespace,
    };

    // Always `Some` by design — a terse constructor for the source-table fields.
    #[allow(clippy::unnecessary_wraps)]
    fn some(s: &str) -> Option<String> {
        Some(s.to_owned())
    }

    #[test]
    fn namespace_prefers_config_over_every_other_source() {
        let sources = NamespaceSources {
            config: some("from-config"),
            pod_namespace_env: some("from-env"),
            service_account_file: some("from-sa"),
            kubeconfig_context: some("from-kubeconfig"),
        };
        assert_eq!(resolve_namespace(&sources).unwrap(), "from-config");
    }

    #[test]
    fn namespace_falls_through_the_source_order() {
        // env wins when config is absent.
        let env = NamespaceSources {
            pod_namespace_env: some("from-env"),
            service_account_file: some("from-sa"),
            kubeconfig_context: some("from-kubeconfig"),
            ..NamespaceSources::default()
        };
        assert_eq!(resolve_namespace(&env).unwrap(), "from-env");

        // SA file wins when config and env are absent.
        let sa = NamespaceSources {
            service_account_file: some("from-sa"),
            kubeconfig_context: some("from-kubeconfig"),
            ..NamespaceSources::default()
        };
        assert_eq!(resolve_namespace(&sa).unwrap(), "from-sa");

        // kubeconfig context is the last resort.
        let kc = NamespaceSources {
            kubeconfig_context: some("from-kubeconfig"),
            ..NamespaceSources::default()
        };
        assert_eq!(resolve_namespace(&kc).unwrap(), "from-kubeconfig");
    }

    #[test]
    fn blank_sources_are_skipped_and_values_are_trimmed() {
        // A present-but-blank higher-priority source does not shadow a real one.
        let sources = NamespaceSources {
            config: some("   "),
            pod_namespace_env: some("\n"),
            service_account_file: some("  gears\n"),
            kubeconfig_context: some("from-kubeconfig"),
        };
        assert_eq!(resolve_namespace(&sources).unwrap(), "gears");
    }

    #[test]
    fn no_namespace_source_is_a_hard_error_with_no_default_fallback() {
        let err = resolve_namespace(&NamespaceSources::default()).unwrap_err();
        match err {
            cluster_sdk::ClusterError::InvalidConfig { reason } => {
                assert!(reason.contains("namespace"));
                assert!(
                    reason.contains("default"),
                    "must state there is no default fallback"
                );
            }
            other => panic!("expected InvalidConfig, got {other:?}"),
        }
    }

    #[test]
    fn identity_prefers_config_then_pod_name_then_hostname() {
        let all = IdentitySources {
            config: some("from-config"),
            pod_name_env: some("from-pod-name"),
            hostname: some("from-hostname"),
        };
        assert_eq!(
            resolve_identity(&all).unwrap(),
            ("from-config".to_owned(), false)
        );

        let pod = IdentitySources {
            pod_name_env: some("from-pod-name"),
            hostname: some("from-hostname"),
            ..IdentitySources::default()
        };
        assert_eq!(
            resolve_identity(&pod).unwrap(),
            ("from-pod-name".to_owned(), false)
        );

        let host = IdentitySources {
            hostname: some("from-hostname"),
            ..IdentitySources::default()
        };
        assert_eq!(
            resolve_identity(&host).unwrap(),
            ("from-hostname".to_owned(), false)
        );
    }

    #[test]
    fn identity_is_truncated_at_the_limit_and_flags_it() {
        let long = "z".repeat(MAX_IDENTITY_LEN + 50);
        let (identity, truncated) = resolve_identity(&IdentitySources {
            config: Some(long),
            ..IdentitySources::default()
        })
        .unwrap();
        assert_eq!(identity.chars().count(), MAX_IDENTITY_LEN);
        assert!(truncated);

        // Exactly at the limit is not flagged.
        let exact = "z".repeat(MAX_IDENTITY_LEN);
        let (_, truncated) = resolve_identity(&IdentitySources {
            config: Some(exact),
            ..IdentitySources::default()
        })
        .unwrap();
        assert!(!truncated);
    }

    #[test]
    fn identity_truncation_respects_char_boundaries() {
        // A multi-byte codepoint (é) straddling the limit must not be split.
        let long = "\u{e9}".repeat(MAX_IDENTITY_LEN + 10);
        let (identity, truncated) = resolve_identity(&IdentitySources {
            config: Some(long),
            ..IdentitySources::default()
        })
        .unwrap();
        assert!(truncated);
        assert_eq!(identity.chars().count(), MAX_IDENTITY_LEN);
        // Round-trips as valid UTF-8 (no split codepoint) by construction.
        assert!(identity.chars().all(|c| c == '\u{e9}'));
    }

    #[test]
    fn no_identity_source_is_an_error() {
        assert!(resolve_identity(&IdentitySources::default()).is_err());
    }
}
