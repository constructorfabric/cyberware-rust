//! Canonical, byte-reproducible serialization of a secured-audit record for
//! the audit store's OWN per-tenant tamper-evidence hash chain (Slice 6
//! Phase 2 Group 2A). Distinct from the posting chain in [`super::chain`]: it
//! has its own domain-separation tag ([`AUDIT_DOMAIN_SEP`]) and its own tip
//! table (`audit_chain_state`), so the two chains never collide.
//!
//! Every field is length-prefixed and NULL-safe (via the shared
//! [`super::canonical`] primitives) so two different field boundaries can never
//! collide. The free-form `before_after` jsonb is hashed over its CANONICAL
//! bytes (sorted keys) so a re-serialization with reordered keys yields the
//! same hash. SHA-256 via the FIPS-validated `aws-lc-rs` provider.

use toolkit_macros::domain_model;
use uuid::Uuid;

use super::canonical::{digest32, put, put_i64, put_opt_str, put_opt_uuid, put_str, put_uuid};
use crate::domain::instant::timestamp_micros;
use time::OffsetDateTime;

/// Versioned domain-separation tag for the secured-audit chain; bump only on an
/// intentional re-freeze of the encoding (which also requires regenerating the
/// byte-repro vector in `audit_chain_tests.rs`).
pub const AUDIT_DOMAIN_SEP: &[u8] = b"VHP-BSS-LEDGER-AUDIT-v1\x1f";

/// The hashing input for one secured-audit record. Borrows its string/json
/// fields. Carries `#[domain_model]` because it is a `pub` type in the domain
/// layer (dylint DE0309); it never crosses the repo boundary.
#[domain_model]
pub struct AuditHashInput<'a> {
    pub audit_id: Uuid,
    pub tenant_id: Uuid,
    pub event_type: &'a str,
    pub actor_ref: Option<&'a str>,
    pub reason_code: Option<&'a str>,
    pub correlation_id: Option<Uuid>,
    pub at_utc: OffsetDateTime,
    pub before_after: &'a serde_json::Value,
}

/// The genesis `prev_hash` for a tenant's first audit record. Tenant-bound and
/// never NULL, so a sealed row's `prev_hash` column is always non-NULL.
#[must_use]
pub fn audit_genesis_prev_hash(tenant_id: Uuid) -> [u8; 32] {
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(AUDIT_DOMAIN_SEP);
    put_uuid(&mut buf, tenant_id);
    put_str(&mut buf, "GENESIS");
    digest32(&buf)
}

/// Canonical bytes of a JSON value with object keys **sorted**, independent of
/// the `serde_json` `preserve_order` feature. We cannot assume the default
/// `BTreeMap` ordering: in a monorepo build another crate may enable
/// `preserve_order` (making `serde_json::Map` an insertion-ordered `IndexMap`),
/// which would otherwise make this hash depend on key insertion order. We first
/// rebuild the value with recursively sorted-key objects, so the serialized
/// bytes are deterministic and canonical in either build — byte-identical to the
/// historical sorted encoding (the domain-sep vector is unchanged).
///
/// # Errors
/// Returns the `serde_json::Error` if `value` fails to serialize. This is not
/// reachable for an in-memory `Value`, but we propagate the error
/// rather than degrade to `b"{}"`: a silent fallback would make an
/// un-serializable record hash IDENTICALLY to an empty-object record (a
/// collision target). Failing the append is correct — a record whose content
/// cannot be canonicalized must never be sealed into the chain.
fn canonical_json(value: &serde_json::Value) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(&canonicalized(value))
}

/// Recursively rebuild `value` with every object's keys in sorted order. With
/// `preserve_order` off (`BTreeMap`) this re-sorts into a `BTreeMap` (a no-op on
/// ordering); with it on (`IndexMap`) the sorted insertion order is preserved on
/// serialize — either way the emitted keys are sorted. Arrays keep their order
/// (order is semantic in a JSON array); scalars are returned as-is.
fn canonicalized(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<(&String, &serde_json::Value)> = map.iter().collect();
            entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in entries {
                out.insert(k.clone(), canonicalized(v));
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(canonicalized).collect())
        }
        other => other.clone(),
    }
}

/// `row_hash = SHA-256(domain_sep ‖ audit_id ‖ tenant_id ‖ event_type ‖
/// actor_ref ‖ reason_code ‖ correlation_id ‖ at_utc(micros) ‖
/// canonical_json(before_after) ‖ prev_hash)`.
///
/// `prev_hash` is the prior audit record's `row_hash`, or
/// [`audit_genesis_prev_hash`] for a tenant's first record.
///
/// # Errors
/// Returns the `serde_json::Error` from [`canonical_json`] if `before_after`
/// fails to serialize — never hash a record we cannot canonicalize.
pub fn audit_row_hash(
    rec: &AuditHashInput,
    prev_hash: &[u8; 32],
) -> Result<[u8; 32], serde_json::Error> {
    let mut buf = Vec::with_capacity(256);
    buf.extend_from_slice(AUDIT_DOMAIN_SEP);

    put_uuid(&mut buf, rec.audit_id);
    put_uuid(&mut buf, rec.tenant_id);
    put_str(&mut buf, rec.event_type);
    put_opt_str(&mut buf, rec.actor_ref);
    put_opt_str(&mut buf, rec.reason_code);
    put_opt_uuid(&mut buf, rec.correlation_id);
    put_i64(&mut buf, timestamp_micros(rec.at_utc));
    put(&mut buf, &canonical_json(rec.before_after)?);

    put(&mut buf, prev_hash);
    Ok(digest32(&buf))
}

#[cfg(test)]
#[path = "audit_chain_tests.rs"]
mod tests;
