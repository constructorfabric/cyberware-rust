//! Value-fingerprint fence — the crypto and constants binding a secret's
//! backend value to the metadata row that governs its visibility.
//!
//! Every write stamps `credstore_secrets.value_fp` with
//! `HMAC-SHA256(fence_key, value)` in the same transaction that switches the
//! row's `value_id` pointer; every read recomputes it from the value the
//! backend returned and serves the value only when the row agrees. Under
//! ADR-0006's immutable value versions, a row never points at a `value_id`
//! until the bytes under that exact id are already fully written (the
//! backend `put` precedes the row CAS), so a torn write can no longer
//! produce a mismatch — the fence's remaining role is an integrity check: a
//! mismatch now means the backend entry the row currently points to was
//! altered or corrupted out of band. Recovery is an ordinary new write (a
//! fresh `value_id`), never a re-`PUT` of the same bytes into the old
//! key — there is no "same key" left to re-inject into.
//!
//! The fence key is deployment state, not configuration: auto-generated and
//! stored in the value-store backend itself under
//! `(TenantId::nil(), `[`credstore_sdk::FENCE_KEY_VALUE_ID`]`)` — no metadata
//! row ever points at that id, so no API path can resolve, overwrite, or
//! delete it. Split knowledge: fingerprints live in the gear DB, the key
//! lives with the values, so a read-only DB compromise alone cannot
//! dictionary-test fingerprints, and backend compromise yields the
//! plaintexts anyway. The fingerprint itself never leaves the gear (no API
//! field, header, or log line).

use aws_lc_rs::hmac;

use crate::domain::error::DomainError;

/// Fence-key id stamped into `fp_key_id` alongside every fingerprint.
/// v1 uses a single key; the id column plus the reserved-reference naming
/// are the keyring groundwork for rotation (out of scope — see the feature
/// spec §3, `algo-fp-compute-verify`).
pub const CURRENT_FENCE_KEY_ID: i16 = 1;

/// Fence-key length in bytes.
pub const FENCE_KEY_LEN: usize = 32;

/// Max jitter (ms) before the fence-key bootstrap read/generate. De-synchronises
/// replicas that cold-start together so a create collision is unlikely. The
/// plugin port has no atomic create-if-absent, so this is a probabilistic
/// defence, not mutual exclusion; residual races stay fail-closed (mismatch →
/// 404 → self-heal on rewrite). One-time cost on a replica's first fence op.
/// Zero under unit tests: the jitter is a timing-only knob, not logic under
/// test, and real sleeps would make the suite slow and time-sensitive. (Lives
/// here, not in the service module, so its test override does not put a
/// `#[cfg(test)]` item in a file that has a companion `_tests.rs` — DE1101.)
#[cfg(not(test))]
pub const BOOTSTRAP_JITTER_MAX_MS: u64 = 500;
#[cfg(test)]
pub const BOOTSTRAP_JITTER_MAX_MS: u64 = 0;

/// `HMAC-SHA256(fence_key, value)` — the full 32-byte fingerprint stored in
/// `credstore_secrets.value_fp`. Never truncated: the value is internal-only,
/// and truncation would only add fence-collision surface.
///
/// Backed by AWS-LC (`aws-lc-rs`), the same crypto library as the workspace TLS
/// stack; under a `--features fips` build its `fips` feature unifies on, so the
/// MAC runs through the FIPS-validated module (`RustCrypto`'s pure-Rust
/// `sha2`/`hmac`, banned by the DE0708 FIPS-hasher lint, never could).
#[must_use]
pub fn compute_fp(key: &[u8], value: &[u8]) -> Vec<u8> {
    let key = hmac::Key::new(hmac::HMAC_SHA256, key);
    hmac::sign(&key, value).as_ref().to_vec()
}

/// Constant-time fingerprint verification (`aws_lc_rs::hmac::verify`).
#[must_use]
pub fn verify_fp(key: &[u8], value: &[u8], fp: &[u8]) -> bool {
    let key = hmac::Key::new(hmac::HMAC_SHA256, key);
    hmac::verify(&key, value, fp).is_ok()
}

/// Generate a fresh [`FENCE_KEY_LEN`]-byte fence key from the platform CSPRNG
/// (AWS-LC; FIPS-validated under a `--features fips` build, as above).
///
/// # Errors
///
/// Returns [`DomainError::Internal`] if the system RNG fails.
pub fn generate_key() -> Result<Vec<u8>, DomainError> {
    let mut buf = [0u8; FENCE_KEY_LEN];
    aws_lc_rs::rand::fill(&mut buf)
        .map_err(|_| DomainError::internal("fence: system RNG failed to generate a fence key"))?;
    Ok(buf.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fp_is_deterministic_and_key_and_value_sensitive() {
        let k1 = vec![1u8; FENCE_KEY_LEN];
        let k2 = vec![2u8; FENCE_KEY_LEN];
        let a = compute_fp(&k1, b"value");
        assert_eq!(a, compute_fp(&k1, b"value"));
        assert_eq!(a.len(), 32);
        assert_ne!(a, compute_fp(&k2, b"value"));
        assert_ne!(a, compute_fp(&k1, b"other"));
    }

    #[test]
    fn verify_accepts_matching_and_rejects_foreign_fp() {
        let key = generate_key().expect("key");
        let fp = compute_fp(&key, b"secret");
        assert!(verify_fp(&key, b"secret", &fp));
        assert!(!verify_fp(&key, b"tampered", &fp));
        assert!(!verify_fp(&generate_key().expect("key"), b"secret", &fp));
        // Truncated/foreign-length fingerprints must not verify.
        assert!(!verify_fp(&key, b"secret", &fp[..8]));
    }

    #[test]
    fn generated_keys_are_distinct_and_sized() {
        let a = generate_key().expect("key");
        let b = generate_key().expect("key");
        assert_eq!(a.len(), FENCE_KEY_LEN);
        assert_ne!(a, b);
    }
}
