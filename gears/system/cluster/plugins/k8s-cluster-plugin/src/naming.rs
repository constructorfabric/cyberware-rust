//! Object-name mapping and the fixed label/annotation vocabulary (DESIGN.md §2.2,
//! §2.3).
//!
//! Cluster coordination names are scoped (e.g. `event-broker/shard-7/worker-pool`)
//! and are not legal Kubernetes object names. [`lease_name`] maps any coordination
//! name to `<prefix>-<seg>-<slug>-<hash16>`, an **injective** RFC 1123 subdomain:
//! the slug is decorative (readability in `kubectl`), the trailing 16 hex
//! characters of `SHA-256(name)` carry the identity, so two distinct names never
//! collide short of a 64-bit hash collision. The mapping is stable across
//! processes and restarts — it hashes the name string and nothing else.

use cluster_sdk::ClusterError;
use sha2::{Digest, Sha256};

/// Label: one selector finds everything this plugin owns, in any namespace.
pub const LABEL_MANAGED_BY: &str = "cluster.cf-gears.io/managed-by";
/// The constant value of [`LABEL_MANAGED_BY`].
pub const MANAGED_BY_VALUE: &str = "cf-gears-cluster";
/// Label: per-primitive listing without parsing names; the cache watch/scan selector.
pub const LABEL_PRIMITIVE: &str = "cluster.cf-gears.io/primitive";
/// Annotation: the original, unmapped coordination name (the inverse of the mapping).
pub const ANNOTATION_NAME: &str = "cluster.cf-gears.io/name";
/// Annotation (locks only): the exact requested TTL in milliseconds (§2.10).
pub const ANNOTATION_TTL_MS: &str = "cluster.cf-gears.io/ttl-ms";

/// The maximum accepted `lease_prefix` length. Kept well under the RFC 1123 label
/// ceiling (63) so the composed object name always has room for the seg, slug, and
/// hash within the 253-character subdomain budget.
pub const MAX_LEASE_PREFIX_LEN: usize = 40;

/// RFC 1123 subdomain maximum length; the composed object name must not exceed it.
const MAX_OBJECT_NAME_LEN: usize = 253;

/// Hex characters of the name hash carried in every object name. 16 hex = 64 bits:
/// a collision needs ~5e9 distinct names in one namespace to reach a 1e-9 chance.
const HASH_HEX_LEN: usize = 16;

/// The per-primitive name segment, disambiguating the same coordination name used
/// by two different primitives (an election and a lock named `"foo"` map to
/// distinct objects).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seg {
    /// Leader-election Lease.
    Election,
    /// Distributed-lock Lease.
    Lock,
    /// Cache `ClusterCacheEntry`.
    Cache,
}

impl Seg {
    /// The two-character segment embedded in the object name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Election => "el",
            Self::Lock => "lk",
            Self::Cache => "ca",
        }
    }

    /// The `cluster.cf-gears.io/primitive` label value for this segment.
    #[must_use]
    pub fn primitive_label(self) -> &'static str {
        match self {
            Self::Election => "election",
            Self::Lock => "lock",
            Self::Cache => "cache",
        }
    }
}

/// The first [`HASH_HEX_LEN`] lowercase hex characters of `SHA-256(name)`.
///
/// `pub` (crate-visible, since this module is private) so the preflight canary can
/// key its per-instance object on the resolved identity's hash (§3.4) using the
/// same hashing this crate uses for object names — one hash implementation, not two.
pub fn hash16(name: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(name.as_bytes());
    let mut out = String::with_capacity(HASH_HEX_LEN);
    // Two hex chars per byte; HASH_HEX_LEN is even, so this consumes whole bytes.
    for &byte in digest.iter().take(HASH_HEX_LEN.div_ceil(2)) {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// A decorative slug: `name` lowercased, every run of non-`[a-z0-9-]` collapsed to
/// a single `-`, trimmed of leading/trailing `-`, then truncated to `max_len` and
/// re-trimmed so it never ends in `-`. May be empty (a name with no legal
/// characters), in which case [`lease_name`] omits it entirely.
fn slug(name: &str, max_len: usize) -> String {
    let mut out = String::with_capacity(name.len().min(max_len));
    let mut prev_dash = false;
    for ch in name.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_lowercase() || lower.is_ascii_digit() {
            out.push(lower);
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    let mut slug: String = trimmed.chars().take(max_len).collect();
    while slug.ends_with('-') {
        slug.pop();
    }
    slug
}

/// Maps a coordination `name` to a legal, injective Kubernetes object name of the
/// form `<prefix>-<seg>-<slug>-<hash16>`.
///
/// `prefix` is assumed already validated by [`validate_lease_prefix`]. The result
/// is always a legal RFC 1123 subdomain (`<= 253` chars, lowercase). The slug is
/// truncated — never the hash — when the budget is tight, so identity is preserved.
#[must_use]
pub fn lease_name(prefix: &str, seg: Seg, name: &str) -> String {
    let hash = hash16(name);
    // Fixed overhead: prefix + '-' + seg(2) + '-' + '-' + hash(16).
    let fixed = prefix.len() + 1 + seg.as_str().len() + 1 + 1 + hash.len();
    let slug_budget = MAX_OBJECT_NAME_LEN.saturating_sub(fixed);
    let slug = slug(name, slug_budget);
    if slug.is_empty() {
        format!("{prefix}-{}-{hash}", seg.as_str())
    } else {
        format!("{prefix}-{}-{slug}-{hash}", seg.as_str())
    }
}

/// Validates a `lease_prefix`: a non-empty RFC 1123 label (`[a-z0-9]` with interior
/// `-`, alphanumeric ends) of at most [`MAX_LEASE_PREFIX_LEN`] characters.
///
/// This is the plugin's *own* config, validated so the composed object name cannot
/// be illegal at the front. Coordination names themselves are never rejected — they
/// are hashed, and anything can be hashed.
///
/// # Errors
///
/// Returns [`ClusterError::InvalidConfig`] naming the rule when `prefix` is empty,
/// longer than [`MAX_LEASE_PREFIX_LEN`], or not a legal RFC 1123 label (uppercase,
/// `/`, or a non-alphanumeric first/last character).
pub fn validate_lease_prefix(prefix: &str) -> Result<(), ClusterError> {
    let invalid = |reason: &str| {
        Err(ClusterError::InvalidConfig {
            reason: format!("lease_prefix `{prefix}` {reason}"),
        })
    };
    if prefix.is_empty() {
        return invalid("must not be empty");
    }
    if prefix.len() > MAX_LEASE_PREFIX_LEN {
        return invalid(&format!(
            "must be at most {MAX_LEASE_PREFIX_LEN} characters"
        ));
    }
    if !is_rfc1123_label(prefix) {
        return invalid(
            "must be a lowercase RFC 1123 label (a-z, 0-9, '-'; alphanumeric first and last)",
        );
    }
    Ok(())
}

/// Whether `s` is a legal, non-empty RFC 1123 label: `[a-z0-9]([a-z0-9-]*[a-z0-9])?`.
#[must_use]
pub fn is_rfc1123_label(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let bytes = s.as_bytes();
    let alnum = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    if !alnum(bytes[0]) || !alnum(bytes[bytes.len() - 1]) {
        return false;
    }
    bytes
        .iter()
        .all(|&b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::{Seg, is_rfc1123_label, lease_name, validate_lease_prefix};

    const PREFIX: &str = "cluster";

    /// A legal RFC 1123 subdomain: `<= 253` chars, lowercase, dot-separated labels
    /// that are each legal RFC 1123 labels. Our names never contain `.`, so this
    /// reduces to one label check, but the helper is written to the general rule.
    fn is_rfc1123_subdomain(s: &str) -> bool {
        !s.is_empty() && s.len() <= 253 && s.split('.').all(is_rfc1123_label)
    }

    /// The hash is the final `-`-separated segment.
    fn trailing_hash(name: &str) -> &str {
        name.rsplit('-').next().unwrap()
    }

    #[test]
    fn injective_across_separator_variants() {
        // `a/b`, `a-b`, `a_b` must not collide — the property a lossy slug breaks.
        let ab_slash = lease_name(PREFIX, Seg::Election, "a/b");
        let ab_dash = lease_name(PREFIX, Seg::Election, "a-b");
        let ab_under = lease_name(PREFIX, Seg::Election, "a_b");
        assert_ne!(ab_slash, ab_dash);
        assert_ne!(ab_dash, ab_under);
        assert_ne!(ab_slash, ab_under);
    }

    #[test]
    fn distinct_across_segments() {
        let name = "event-broker/worker-pool";
        assert_ne!(
            lease_name(PREFIX, Seg::Election, name),
            lease_name(PREFIX, Seg::Lock, name)
        );
        assert_ne!(
            lease_name(PREFIX, Seg::Lock, name),
            lease_name(PREFIX, Seg::Cache, name)
        );
    }

    #[test]
    fn deterministic() {
        let name = "oagw/tenant-42/rate-limit";
        assert_eq!(
            lease_name(PREFIX, Seg::Lock, name),
            lease_name(PREFIX, Seg::Lock, name)
        );
    }

    #[test]
    fn hash_is_16_lowercase_hex_and_survives_truncation() {
        // A 4 KiB name forces slug truncation; the hash must be intact.
        let name = "x".repeat(4096);
        let mapped = lease_name(PREFIX, Seg::Cache, &name);
        let hash = trailing_hash(&mapped);
        assert_eq!(hash.len(), 16);
        assert!(
            hash.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
        // Same hash on a short name with the same content prefix stays stable.
        assert_eq!(hash, trailing_hash(&lease_name("p", Seg::Cache, &name)));
    }

    #[test]
    fn legal_and_bounded_for_adversarial_inputs() {
        let long = "A/".repeat(3000); // uppercase + '/' + 6 KiB
        for name in [
            "a/b",
            "A/B/C",
            "\u{e9}v\u{e9}nement/2024", // unicode (é)
            "/leading/and/trailing/",
            "already-legal-name",
            &"z".repeat(4096),
            &long,
            "---", // all separators → empty slug
            "42",  // digits only
        ] {
            let mapped = lease_name(&"p".repeat(40), Seg::Lock, name);
            assert!(
                is_rfc1123_subdomain(&mapped),
                "not a legal subdomain: {mapped}"
            );
            assert!(
                mapped.len() <= 253,
                "over 253: {} for {name:?}",
                mapped.len()
            );
            assert_eq!(mapped, mapped.to_ascii_lowercase());
        }
    }

    #[test]
    fn empty_slug_omits_the_slug_segment() {
        // A name with no legal characters yields `<prefix>-<seg>-<hash>`.
        let mapped = lease_name(PREFIX, Seg::Election, "///");
        let hash = trailing_hash(&mapped);
        assert_eq!(mapped, format!("cluster-el-{hash}"));
    }

    #[test]
    fn prefix_validation() {
        assert!(validate_lease_prefix("cluster").is_ok());
        assert!(validate_lease_prefix("c").is_ok());
        assert!(validate_lease_prefix("a1-b2").is_ok());
        assert!(validate_lease_prefix("").is_err()); // empty
        assert!(validate_lease_prefix("Cluster").is_err()); // uppercase
        assert!(validate_lease_prefix("clu/ster").is_err()); // slash
        assert!(validate_lease_prefix("-cluster").is_err()); // leading dash
        assert!(validate_lease_prefix("cluster-").is_err()); // trailing dash
        assert!(validate_lease_prefix(&"p".repeat(41)).is_err()); // too long
        assert!(validate_lease_prefix(&"p".repeat(40)).is_ok()); // at the limit
    }
}
