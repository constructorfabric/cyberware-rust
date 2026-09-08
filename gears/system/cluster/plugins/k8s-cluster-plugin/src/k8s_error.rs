//! `kube::Error` / HTTP status → `ClusterError` / `ProviderErrorKind` (DESIGN.md §10).
//!
//! This is the generic transport/status mapper. Status codes that are classified
//! *per primitive* — `409` (contention / CAS / already-exists), `404` (absent),
//! `410 Gone` (watch re-list), and a cache `422` (CRD schema skew) — are intercepted
//! in `guarded.rs` and the cache/lock/leader code **before** they reach here; this
//! mapper is the fallback for everything else, and classifies any unrecognised code
//! as the non-retryable [`ProviderErrorKind::Other`].

use cluster_sdk::{ClusterError, ProviderErrorKind};

/// Builds a `ClusterError::Provider` of `kind` with a human-readable `message`.
fn provider(kind: ProviderErrorKind, message: impl std::fmt::Display) -> ClusterError {
    ClusterError::Provider {
        kind,
        message: message.to_string(),
    }
}

/// Maps an API-server `Status` (the body of `kube::Error::Api`) to a `ClusterError`
/// by its HTTP code, per the §10 table.
pub fn map_api_status(status: &kube::core::Status) -> ClusterError {
    match status.code {
        // Auth failure mid-flight means RBAC changed under a running gear; retrying
        // cannot fix it, so this is deliberately not retryable.
        401 | 403 => provider(ProviderErrorKind::AuthFailure, status),
        // 429: API Priority and Fairness backpressure (the caller honours
        // `Retry-After`). 500/503: API server / etcd unavailable, quorum loss.
        // All three are retryable with backoff.
        429 | 500 | 503 => provider(ProviderErrorKind::ResourceExhausted, status),
        504 => provider(ProviderErrorKind::Timeout, status),
        // 404/409/410, and a cache-write 422, are handled per primitive before
        // reaching this mapper; anything else is a non-retryable Other.
        _ => provider(ProviderErrorKind::Other, status),
    }
}

/// Maps a `kube::Error` to a `ClusterError` (DESIGN.md §10).
///
/// Config-inference failures (a malformed kubeconfig, an unresolvable in-cluster
/// config) map to [`ClusterError::InvalidConfig`] — an operator should look at their
/// YAML or pod spec, not their cluster. Transport failures map to
/// [`ProviderErrorKind::ConnectionLost`]. See [`map_api_status`] for the HTTP-code
/// rows.
pub fn map_kube_error(err: &kube::Error) -> ClusterError {
    use kube::Error as E;
    match err {
        E::Api(status) => map_api_status(status),
        E::HyperError(_) | E::Service(_) | E::ReadEvents(_) => {
            provider(ProviderErrorKind::ConnectionLost, err)
        }
        E::Auth(_) => provider(ProviderErrorKind::AuthFailure, err),
        // A malformed kubeconfig or an unresolvable in-cluster config is an operator
        // configuration fault, not a runtime backend fault (§10, §3.6).
        E::InferConfig(_) | E::InferKubeconfig(_) => ClusterError::InvalidConfig {
            reason: err.to_string(),
        },
        // Everything else (request-build failures, TLS/serde faults, ...) is a
        // non-retryable Other.
        _ => provider(ProviderErrorKind::Other, err),
    }
}

/// The `ClusterError` for a client-side request timeout (`request_timeout_ms`
/// elapsed, surfaced as a `tokio::time::error::Elapsed`, not a `kube::Error`). §10.
pub fn timeout(context: impl std::fmt::Display) -> ClusterError {
    provider(ProviderErrorKind::Timeout, context)
}

#[cfg(test)]
mod tests {
    use super::map_api_status;
    use cluster_sdk::{ClusterError, ProviderErrorKind};
    use kube::core::Status;

    fn status(code: u16) -> Status {
        Status {
            code,
            reason: format!("code-{code}"),
            ..Default::default()
        }
    }

    fn kind_of(err: &ClusterError) -> ProviderErrorKind {
        match err {
            ClusterError::Provider { kind, .. } => *kind,
            other => panic!("expected Provider, got {other:?}"),
        }
    }

    #[test]
    fn http_status_mapping_table() {
        use ProviderErrorKind::{AuthFailure, Other, ResourceExhausted, Timeout};
        let cases = [
            (401, AuthFailure),
            (403, AuthFailure),
            (429, ResourceExhausted),
            (500, ResourceExhausted),
            (503, ResourceExhausted),
            (504, Timeout),
            // Fallback rows — normally intercepted per-primitive, non-retryable here.
            (422, Other),
            (404, Other),
            (409, Other),
        ];
        for (code, expected) in cases {
            assert_eq!(
                kind_of(&map_api_status(&status(code))),
                expected,
                "code {code}"
            );
        }
    }

    #[test]
    fn auth_failure_is_not_retryable() {
        assert!(!map_api_status(&status(403)).is_retryable());
        assert!(map_api_status(&status(429)).is_retryable());
        assert!(map_api_status(&status(503)).is_retryable());
    }
}
