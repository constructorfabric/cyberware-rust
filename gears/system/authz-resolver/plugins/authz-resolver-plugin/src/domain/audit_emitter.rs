//! Structured audit logging for completed evaluations.
//!
//! `AuditEmitter` emits one `tracing::info!(target = "cf-authz.audit", …)`
//! event per `Ok(EvaluationResponse)` (allow OR business deny). `Err(_)`
//! infrastructure paths are not audited — they belong in tracing logs and
//! metrics. Gated by `config.audit.enabled` (default `false`).
//!
//! Sensitive-data exclusion is structural: `AuditRecord` has no
//! `bearer_token` field; raw constraint predicates never reach the record
//! (only `constraints_count` + `constraints_hash`).
//!
//! `constraints_hash` is a 16-hex-char FNV-1a fingerprint of
//! `serde_json::to_string(&constraints)`.

use std::time::Duration;

use authz_resolver_sdk::EvaluationRequest;
use authz_resolver_sdk::constraints::Constraint;
use authz_resolver_sdk::models::{DenyReason, EvaluationResponse};
use toolkit_macros::domain_model;
use tracing::{info, warn};
use uuid::Uuid;

/// Decision-focused audit record. Mandatory fields are populated for every
/// emitted event; conditional fields depend on `decision`:
/// - `deny_reason` is `Some(_)` only when `decision == false`.
/// - `constraints_count` / `constraints_hash` are `Some(_)` only when
///   `decision == true`. `constraints_hash` is `None` when the count is 0.
#[domain_model]
#[derive(Debug, Clone)]
pub(crate) struct AuditRecord {
    pub correlation_id: Uuid,
    pub subject_id: Uuid,
    /// Caller-asserted subject tenant (from `subject.properties`), for the
    /// audit trail only — never consulted for authorization. See
    /// `from_response` for the security note.
    pub subject_tenant_id: Option<Uuid>,
    pub action: String,
    pub resource_type: String,
    pub resource_id: Option<Uuid>,
    pub decision: bool,
    pub latency_ms: u64,
    pub deny_reason: Option<DenyReason>,
    pub constraints_count: Option<usize>,
    pub constraints_hash: Option<String>,
}

impl AuditRecord {
    /// Build a record from an outgoing `EvaluationResponse`. Pure assembly:
    /// no I/O, no allocation beyond field copies + the hash computation.
    pub(crate) fn from_response(
        correlation_id: Uuid,
        request: &EvaluationRequest,
        latency: Duration,
        response: &EvaluationResponse,
    ) -> Self {
        // Caller-ASSERTED subject tenant, for the audit trail only. It comes
        // from `subject.properties` (set by AuthN), so it is NOT a trusted
        // value the plugin verified. SECURITY: authorization never reads this
        // — tenant scoping is driven solely by the RBAC-returned scope and the
        // Tenant Resolver's authoritative hierarchy (see hierarchy_client).
        // Do not route this into any decision path.
        let subject_tenant_id = request
            .subject
            .properties
            .get("tenant_id")
            .and_then(serde_json::Value::as_str)
            .and_then(|s| Uuid::parse_str(s).ok());

        let (deny_reason, constraints_count, constraints_hash) = if response.decision {
            let count = response.context.constraints.len();
            let hash = compute_constraints_hash(&response.context.constraints);
            (None, Some(count), hash)
        } else {
            (response.context.deny_reason.clone(), None, None)
        };

        Self {
            correlation_id,
            subject_id: request.subject.id,
            subject_tenant_id,
            action: request.action.name.clone(),
            resource_type: request.resource.resource_type.clone(),
            resource_id: request.resource.id,
            decision: response.decision,
            latency_ms: u64::try_from(latency.as_millis()).unwrap_or(u64::MAX),
            deny_reason,
            constraints_count,
            constraints_hash,
        }
    }
}

/// Compute a 16-hex-char FNV-1a fingerprint of the JSON-serialized
/// constraints. Returns `None` for empty input.
///
/// FNV-1a 64-bit is a deterministic, non-cryptographic fingerprint (DE0708: no
/// non-FIPS hashers). The hash is audit-diagnostic only — not a security or
/// integrity boundary.
fn compute_constraints_hash(constraints: &[Constraint]) -> Option<String> {
    if constraints.is_empty() {
        return None;
    }
    let mut hasher = FnvWriter::new();
    if let Err(err) = serde_json::to_writer(&mut hasher, constraints) {
        warn!(error = %err, "failed to serialize constraints for audit hash");
        return None;
    }
    Some(format!("{:016x}", hasher.finish()))
}

/// FNV-1a 64-bit state behind an [`std::io::Write`] sink.
///
/// `serde_json::to_writer` streams into this, so the constraint set is never
/// materialized as a `String` first. That mattered: a constraint may carry up
/// to `max_expansion_ids` (default `10_000`) UUIDs, so the intermediate JSON
/// was hundreds of KB allocated per audited decision purely to be hashed and
/// dropped.
///
/// Folding the same bytes in the same order yields the **same digest** the
/// string-based version produced, so audit hashes stay comparable across this
/// change — `fnv_writer_matches_hashing_the_json_string` pins that.
struct FnvWriter {
    hash: u64,
}

impl FnvWriter {
    const BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01B3;

    const fn new() -> Self {
        Self { hash: Self::BASIS }
    }

    const fn finish(&self) -> u64 {
        self.hash
    }
}

impl std::io::Write for FnvWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        for &b in buf {
            self.hash ^= u64::from(b);
            self.hash = self.hash.wrapping_mul(Self::PRIME);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Gated audit emitter. Synchronous from the caller's perspective; tracing
/// layers handle the actual transport.
#[domain_model]
pub(crate) struct AuditEmitter {
    enabled: bool,
}

impl AuditEmitter {
    pub(crate) fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    /// Emit one structured tracing event for the supplied record. When
    /// `enabled = false`, the call is a no-op.
    pub(crate) fn emit(&self, record: &AuditRecord) {
        if !self.enabled {
            return;
        }
        let deny_error_code = record
            .deny_reason
            .as_ref()
            .map_or("", |d| d.error_code.as_str());
        let deny_details = record
            .deny_reason
            .as_ref()
            .and_then(|d| d.details.as_deref())
            .unwrap_or("");
        // Render `Option` fields as typed values, not `?` Debug ("Some(..)" /
        // "None"). Log consumers and tests then read a typed number / string
        // instead of parsing a Debug rendering. Absent values use a typed
        // empty/zero default rather than the string "None".
        let subject_tenant_id = record
            .subject_tenant_id
            .map(|id| id.to_string())
            .unwrap_or_default();
        let resource_id = record
            .resource_id
            .map(|id| id.to_string())
            .unwrap_or_default();
        let constraints_count = record.constraints_count.unwrap_or(0) as u64;
        let constraints_hash = record.constraints_hash.as_deref().unwrap_or("");
        // SECURITY: `action`, `resource_type`, and `deny_details` are derived
        // from caller-controlled request fields (validation only rejects `*`/`?`
        // in action.name, never control characters), so an embedded CR/LF could
        // forge or split an audit line under a line-oriented (text) tracing
        // subscriber. Neutralize control chars before emitting.
        let action = sanitize_for_audit(&record.action);
        let resource_type = sanitize_for_audit(&record.resource_type);
        let deny_details = sanitize_for_audit(deny_details);
        info!(
            target: "cf-authz.audit",
            correlation_id = %record.correlation_id,
            subject_id = %record.subject_id,
            subject_tenant_id = %subject_tenant_id,
            action = %action,
            resource_type = %resource_type,
            resource_id = %resource_id,
            decision = record.decision,
            latency_ms = record.latency_ms,
            deny_error_code = %deny_error_code,
            deny_details = %deny_details,
            constraints_count = constraints_count,
            constraints_hash = %constraints_hash,
            "authz evaluation"
        );
    }
}

/// Neutralize log-injection vectors in request-derived audit fields: replace
/// any control character (CR, LF, tab, ANSI ESC, other C0/C1) with the Unicode
/// replacement character and cap the length. Benign values (no control chars,
/// under the cap) pass through unchanged, so normal audit output is unaffected.
fn sanitize_for_audit(value: &str) -> String {
    /// Generous bound: real action/resource-type ids are short; this only
    /// guards against an attacker padding an audit line to absurd length.
    const MAX_LEN: usize = 256;
    value
        .chars()
        .map(|c| if c.is_control() { '\u{FFFD}' } else { c })
        .take(MAX_LEN)
        .collect()
}

#[cfg(test)]
#[path = "audit_emitter_tests.rs"]
mod tests;
