//! The `ClusterCacheEntry` custom resource and its projection to/from
//! [`CacheEntry`] (DESIGN.md §2.7).
//!
//! One namespaced object per cache key. `spec.version` is the cluster cache version
//! — a field this plugin owns and increments (§2.8), never `metadata.resourceVersion`
//! — so it starts at 1, increases on every write, and resets to 1 on
//! delete-and-recreate (which `SC-CACHE-009` requires). `spec.value` is base64
//! (`format: byte`); `spec.expiresAt` is an absolute RFC 3339 deadline, absent for
//! [`Ttl::Indefinite`].

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use cluster_sdk::cache::CacheEntry;
use cluster_sdk::{ClusterError, ProviderErrorKind};
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The `ClusterCacheEntry.spec`. The derive generates the `ClusterCacheEntry`
/// root type (with `metadata` + `spec`) and its `kube::Resource` /
/// `CustomResourceExt` impls.
#[derive(CustomResource, Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[kube(
    group = "cluster.cf-gears.io",
    version = "v1",
    kind = "ClusterCacheEntry",
    plural = "clustercacheentries",
    singular = "clustercacheentry",
    shortname = "cce",
    namespaced
)]
#[serde(rename_all = "camelCase")]
pub struct ClusterCacheEntrySpec {
    /// The stored bytes, base64-encoded (`format: byte`).
    pub value: String,
    /// The monotonic cache version (`>= 1`); this plugin's own field (§2.8).
    pub version: i64,
    /// Absolute expiry deadline (RFC 3339). Absent means [`Ttl::Indefinite`](cluster_sdk::cache::Ttl::Indefinite).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

impl ClusterCacheEntrySpec {
    /// Builds a spec from raw value bytes, a version, and an optional expiry.
    #[must_use]
    pub fn new(value: &[u8], version: u64, expires_at: Option<String>) -> Self {
        Self {
            value: BASE64.encode(value),
            version: i64::try_from(version).unwrap_or(i64::MAX),
            expires_at,
        }
    }

    /// Projects the spec's value and version into a [`CacheEntry`].
    ///
    /// # Errors
    ///
    /// Returns [`ClusterError::Provider`] with [`ProviderErrorKind::Other`] when
    /// `spec.value` is not valid base64 — a malformed object (hand-edited, or written
    /// by an incompatible writer) rather than a retryable backend fault.
    pub fn to_cache_entry(&self) -> Result<CacheEntry, ClusterError> {
        let value = BASE64
            .decode(&self.value)
            .map_err(|e| ClusterError::Provider {
                kind: ProviderErrorKind::Other,
                message: format!("ClusterCacheEntry.spec.value is not valid base64: {e}"),
            })?;
        Ok(CacheEntry {
            value,
            // `version` is `>= 1` by the CRD schema; a non-positive value is a
            // corrupt object, floored to 0 (the reserved absent-sentinel).
            version: u64::try_from(self.version).unwrap_or(0),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{ClusterCacheEntry, ClusterCacheEntrySpec};
    use kube::Resource as _;

    #[test]
    fn value_base64_round_trips_arbitrary_bytes() {
        for bytes in [
            Vec::new(),
            vec![0x00, 0xFF, 0xFE, 0x01],
            vec![0xFF; 300],
            b"shard-owner=broker-7".to_vec(),
        ] {
            let spec = ClusterCacheEntrySpec::new(&bytes, 3, None);
            let entry = spec.to_cache_entry().unwrap();
            assert_eq!(entry.value, bytes);
            assert_eq!(entry.version, 3);
        }
    }

    #[test]
    fn expires_at_indefinite_and_present_round_trip() {
        let indefinite = ClusterCacheEntrySpec::new(b"v", 1, None);
        assert!(indefinite.expires_at.is_none());
        // Serialised, the absent expiry is omitted entirely.
        let json = serde_json::to_value(&indefinite).unwrap();
        assert!(json.get("expiresAt").is_none());

        let deadline = "2026-08-06T09:14:52.114Z".to_owned();
        let present = ClusterCacheEntrySpec::new(b"v", 2, Some(deadline.clone()));
        let json = serde_json::to_value(&present).unwrap();
        assert_eq!(json.get("expiresAt").unwrap(), &serde_json::json!(deadline));
    }

    #[test]
    fn malformed_base64_is_a_provider_other_error() {
        let spec = ClusterCacheEntrySpec {
            value: "not valid base64!!!".to_owned(),
            version: 1,
            expires_at: None,
        };
        let err = spec.to_cache_entry().unwrap_err();
        assert!(matches!(
            err,
            cluster_sdk::ClusterError::Provider {
                kind: cluster_sdk::ProviderErrorKind::Other,
                ..
            }
        ));
    }

    /// The derived type and the shipped manifest must agree on the identifying
    /// metadata — a rename on either side is a breaking wire change (§2.7).
    #[test]
    fn crd_manifest_matches_derived_type() {
        // Derived `kube::Resource` metadata (DynamicType = ()).
        assert_eq!(ClusterCacheEntry::group(&()), "cluster.cf-gears.io");
        assert_eq!(ClusterCacheEntry::version(&()), "v1");
        assert_eq!(ClusterCacheEntry::kind(&()), "ClusterCacheEntry");
        assert_eq!(ClusterCacheEntry::plural(&()), "clustercacheentries");

        // The shipped manifest carries the same values.
        let manifest = include_str!("../../deploy/crd.yaml");
        for needle in [
            "group: cluster.cf-gears.io",
            "kind: ClusterCacheEntry",
            "plural: clustercacheentries",
            "singular: clustercacheentry",
            "scope: Namespaced",
            "name: v1",
        ] {
            assert!(manifest.contains(needle), "crd.yaml missing `{needle}`");
        }
    }
}
