//! Canonical, byte-reproducible serialization of a posted entry for the
//! per-tenant tamper-evidence hash chain (Slice 6, design §4.2).
//!
//! Every field is length-prefixed and NULL-safe so two different field
//! boundaries can never collide; PII, `correlation_id`, and the free-form
//! `rounding_evidence` jsonb are **excluded** (so GDPR erasure never breaks
//! the chain). The financially-binding AR sub-class `ar_status` (Slice 2, set
//! on chargeback/dispute reclass lines) **is** covered (design §4.2, Rev2 B-8).
//! The encoding still **reserves** `rate_snapshot_ref` (Slice 5) and the
//! per-line `legal_entity_id` override as NULL slots so the byte format never
//! re-freezes when those slices populate them (Rev2 B-8 / R-9). Lines are
//! hashed in `line_id` order (matches the Verifier re-walk). SHA-256 via the
//! FIPS-validated `aws-lc-rs` provider — `sha2` is blocked by dylint DE0708.

use chrono::Datelike;
use uuid::Uuid;

use super::canonical::{
    digest32, put, put_i32, put_i64, put_none, put_opt_i64, put_opt_str, put_opt_uuid, put_str,
    put_uuid,
};
use crate::domain::instant::timestamp_micros;
use crate::domain::model::{NewEntry, NewLine};

/// Versioned domain-separation tag; bump only on an intentional re-freeze of
/// the encoding (which also requires regenerating the §11 byte-repro vector).
pub const CHAIN_DOMAIN_SEP: &[u8] = b"VHP-BSS-LEDGER-CHAIN-v2\x1f";

/// The genesis `prev_hash` for a tenant's first posted entry. Tenant-bound and
/// never NULL, so a sealed row's `prev_hash` column is always non-NULL.
#[must_use]
pub fn genesis_prev_hash(tenant_id: Uuid) -> [u8; 32] {
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(CHAIN_DOMAIN_SEP);
    put_uuid(&mut buf, tenant_id);
    put_str(&mut buf, "GENESIS");
    digest32(&buf)
}

/// `row_hash = SHA-256(domain_sep ‖ entry fields ‖ lines(line_id order) ‖ prev_hash)`.
///
/// `prev_hash` is the prior entry's `row_hash`, or [`genesis_prev_hash`] for a
/// tenant's first entry.
#[must_use]
pub fn chain_row_hash(entry: &NewEntry, lines: &[NewLine], prev_hash: &[u8; 32]) -> [u8; 32] {
    let mut buf = Vec::with_capacity(512);
    buf.extend_from_slice(CHAIN_DOMAIN_SEP);

    // --- entry fields (design §4.2 order; correlation_id + rounding_evidence EXCLUDED) ---
    put_uuid(&mut buf, entry.tenant_id);
    put_uuid(&mut buf, entry.entry_id);
    put_str(&mut buf, &entry.period_id);
    put_uuid(&mut buf, entry.legal_entity_id);
    put_str(&mut buf, &entry.entry_currency);
    put_str(&mut buf, entry.source_doc_type.as_str());
    put_str(&mut buf, &entry.source_business_id);
    put_opt_uuid(&mut buf, entry.reverses_entry_id);
    put_opt_str(&mut buf, entry.reverses_period_id.as_deref());
    put_i32(&mut buf, entry.effective_at.num_days_from_ce());
    put_i64(&mut buf, timestamp_micros(entry.posted_at_utc));
    put_str(&mut buf, &entry.origin);
    put_uuid(&mut buf, entry.posted_by_actor_id);

    // --- lines in line_id order ---
    let mut ordered: Vec<&NewLine> = lines.iter().collect();
    ordered.sort_by_key(|l| l.line_id);
    let count = u64::try_from(ordered.len()).unwrap_or(u64::MAX);
    put(&mut buf, &count.to_be_bytes());
    for l in ordered {
        put_uuid(&mut buf, l.line_id);
        put_uuid(&mut buf, l.account_id);
        put_str(&mut buf, l.account_class.as_str());
        put_opt_str(&mut buf, l.gl_code.as_deref());
        put_str(&mut buf, l.side.as_str());
        put_i64(&mut buf, l.amount_minor);
        put_str(&mut buf, &l.currency);
        put(&mut buf, &[l.currency_scale]);
        put_opt_i64(&mut buf, l.functional_amount_minor);
        put_opt_str(&mut buf, l.functional_currency.as_deref());
        put_uuid(&mut buf, l.payer_tenant_id);
        put_opt_uuid(&mut buf, l.seller_tenant_id);
        put_opt_uuid(&mut buf, l.resource_tenant_id);
        put_opt_str(&mut buf, l.invoice_id.as_deref());
        put_opt_str(&mut buf, l.revenue_stream.as_deref());
        put_opt_str(&mut buf, l.tax_jurisdiction.as_deref());
        put_opt_str(&mut buf, l.tax_filing_period.as_deref());
        put_opt_str(&mut buf, l.tax_rate_ref.as_deref());
        put_opt_str(&mut buf, l.ar_status.as_deref()); // Slice 2 AR dispute sub-class (chargeback reclass) — financially binding
        put_str(&mut buf, l.mapping_status.as_str());
        put_none(&mut buf); // RESERVED: rate_snapshot_ref (Slice 5) — always NULL in v1
        put_opt_str(&mut buf, l.credit_grant_event_type.as_deref());
        put_opt_str(&mut buf, l.invoice_item_ref.as_deref());
        put_opt_str(&mut buf, l.sku_or_plan_ref.as_deref());
        put_opt_str(&mut buf, l.price_id.as_deref());
        put_opt_str(&mut buf, l.pricing_snapshot_ref.as_deref());
        put_opt_str(&mut buf, l.po_allocation_group.as_deref());
        put_opt_uuid(&mut buf, l.legal_entity_id); // reserved per-line override; NULL in v1
    }

    // --- prev link ---
    put(&mut buf, prev_hash);
    digest32(&buf)
}

#[cfg(test)]
#[path = "chain_tests.rs"]
mod tests;
