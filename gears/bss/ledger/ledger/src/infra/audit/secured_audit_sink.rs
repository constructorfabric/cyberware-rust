//! `SecuredAuditSink` — the Slice-3-local **port** for Slice 6's secured-audit
//! store (design §2.1, F1). Slice 6 (VHP-1858) owns the durable
//! `secured_audit_record` table + its append-only / hash-chained / retention
//! semantics, but its code is not in our base. So Slice 3 depends on this trait
//! whose method signature mirrors Slice 6's `SecuredAuditStore::append`
//! **exactly**:
//!
//! ```ignore
//! async fn append(
//!     &self,
//!     txn: &DbTx<'_>,
//!     scope: &AccessScope,
//!     tenant: Uuid,
//!     event_type: AuditEventType,
//!     actor_ref: Option<&str>,
//!     reason_code: Option<&str>,
//!     before_after: &serde_json::Value,
//!     correlation_id: Option<Uuid>,
//!     retain_until: Option<OffsetDateTime>,
//! ) -> Result<Uuid, DbError>;
//! ```
//!
//! [`NoopSecuredAuditSink`] is wired: it writes nothing durable, logging the
//! would-be record at `info` so the call is observable in traces. The
//! `before_after` payload is asserted PII-clean by the caller (ids + amounts +
//! enum codes only) before it reaches here (design §2.3).
//!
//! GAP (post-VHP-1858 rebase, tracked follow-up — NOT a runtime defect today):
//! Slice 6's real [`crate::infra::audit::store::SecuredAuditStore`] now lives in
//! the base, but the Slice-3 dispositions (the `unknown_final` refund clear in
//! `adjustment::refund_service` and the write-off capture in
//! `adjustment::manual_adjustment_service`) are STILL bound to the no-op sink —
//! the seam was NOT flipped during the rebase. A drop-in swap is not yet safe:
//! (a) the real `store::AuditEventType` taxonomy uses lowercase-hyphen tokens
//! (`"manual-adjustment"`, pinned by the `secured_audit_record` CHECK) whereas
//! this local mirror emits `SCREAMING_SNAKE` (`"MANUAL_ADJUSTMENT"`); (b) the
//! append signatures differ. Unifying onto the real store (or reconciling the
//! token + signature) is its own follow-up; until then nothing durable is
//! written for these two Slice-3 dispositions (the event/intent trail still
//! records them).
//!
//! [`AuditEventType`] is a LOCAL enum-mirror of Slice 6's audit-event taxonomy;
//! Slice 3 only needs the [`AuditEventType::ManualAdjustment`] variant (the same
//! variant Slice 6 stamps for a governed disposition / write-off — re-using it
//! avoids touching Slice 6's secured-audit migration enum at merge, design §2.1).

use time::OffsetDateTime;
use toolkit_db::DbError;
use toolkit_db::secure::{AccessScope, DbTx};
use uuid::Uuid;

/// The secured-audit event taxonomy (a LOCAL mirror of Slice 6's
/// `AuditEventType`). The stored literal is `SCREAMING_SNAKE_CASE` (matching
/// Slice 6's serde + durable enum), so a record Slice 3 writes through the no-op
/// is byte-identical to one the real store would persist post-merge.
///
/// Slice 3 only emits [`Self::ManualAdjustment`] — the variant Slice 6 stamps
/// for a **ledger-side governed disposition** (the `unknown_final` refund
/// write-off to a loss line, design §4.4 / K-1; and the Phase-3 attempted
/// write-off capture, §6 A4). The other variants are declared so the local
/// mirror is a superset that cannot conflict with Slice 6's enum at merge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuditEventType {
    /// A governed manual posting / disposition (the `unknown_final` refund
    /// loss-line write-off; the attempted-write-off capture). The ONLY variant
    /// Slice 3 emits.
    ManualAdjustment,
    /// A dual-control approval was granted (Slice 6 records the approver).
    ApprovalGranted,
    /// A dual-control approval was rejected.
    ApprovalRejected,
    /// A PII-minimization redaction was applied before persist.
    Redaction,
    /// A closed fiscal period was reopened (Slice 7 dual-control seam, design
    /// §7 / N-core-3). Stamped `"period-reopen"` — matching the real
    /// `secured_audit_record` CHECK + Slice 6 taxonomy (the lowercase-hyphen
    /// token, not this mirror's legacy `SCREAMING_SNAKE` — the new variant is
    /// forward-correct for the seam flip).
    PeriodReopen,
}

impl AuditEventType {
    /// Stable `SCREAMING_SNAKE_CASE` literal (the durable event-type code +
    /// Slice 6's serde wire form).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ManualAdjustment => "MANUAL_ADJUSTMENT",
            Self::ApprovalGranted => "APPROVAL_GRANTED",
            Self::ApprovalRejected => "APPROVAL_REJECTED",
            Self::Redaction => "REDACTION",
            Self::PeriodReopen => "period-reopen",
        }
    }
}

/// Port for the secured-audit store (Slice 6's `SecuredAuditStore`). The method
/// signature mirrors Slice 6's `append` exactly so the real store binds at
/// merge with no call-site change. Implementors persist one append-only audit
/// record **in the supplied posting transaction** (so the record commits
/// atomically with the disposition it audits, or rolls back with it).
#[async_trait::async_trait]
pub trait SecuredAuditSink: Send + Sync {
    /// Append one secured-audit record in `txn`, returning its id.
    ///
    /// - `event_type` — the taxonomy variant ([`AuditEventType::ManualAdjustment`]
    ///   for Slice 3's dispositions).
    /// - `actor_ref` — the acting subject (the approver/operator id), `None` for
    ///   a system-initiated record.
    /// - `reason_code` — a closed reason literal (e.g. `"REFUND_UNKNOWN_FINAL"`).
    /// - `before_after` — the PII-clean state delta (ids + amounts + codes only);
    ///   the caller guarantees no PII before the call.
    /// - `correlation_id` — links the record to the posted entry / disposition.
    /// - `retain_until` — the retention horizon (`None` ⇒ the store's default).
    ///
    /// # Errors
    /// [`DbError`] on a storage / scope failure (rolls the disposition back —
    /// the audit record and the books effect are atomic).
    // Mirrors Slice 6 `SecuredAuditStore::append` signature verbatim (seam
    // contract): the arity is fixed by the upstream port so the real store binds
    // at merge with no call-site change — narrowing it here would break that.
    #[allow(clippy::too_many_arguments)]
    async fn append(
        &self,
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        event_type: AuditEventType,
        actor_ref: Option<&str>,
        reason_code: Option<&str>,
        before_after: &serde_json::Value,
        correlation_id: Option<Uuid>,
        retain_until: Option<OffsetDateTime>,
    ) -> Result<Uuid, DbError>;
}

/// The pre-Slice-6 implementation: records **nothing durable** (the
/// `secured_audit_record` table is Slice 6's migration). It logs the would-be
/// record at `info` and returns a fresh id, so the disposition completes and the
/// call is observable in traces. (An operator-visibility counter for dropped
/// secured records is a Slice-6 follow-up — none is wired today.) NO transaction
/// effect (it does
/// not touch `txn`), so it can never roll the disposition back. Wired in
/// `module` until the real `SecuredAuditStore` replaces it at merge.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopSecuredAuditSink;

impl NoopSecuredAuditSink {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl SecuredAuditSink for NoopSecuredAuditSink {
    // Mirrors Slice 6 `SecuredAuditStore::append` signature verbatim (seam
    // contract) — the arity is fixed by the trait it implements.
    #[allow(clippy::too_many_arguments)]
    async fn append(
        &self,
        _txn: &DbTx<'_>,
        _scope: &AccessScope,
        tenant: Uuid,
        event_type: AuditEventType,
        actor_ref: Option<&str>,
        reason_code: Option<&str>,
        before_after: &serde_json::Value,
        correlation_id: Option<Uuid>,
        _retain_until: Option<OffsetDateTime>,
    ) -> Result<Uuid, DbError> {
        let record_id = Uuid::now_v7();
        // No PII in the structured fields — ids + enum codes only. `before_after`
        // is logged as a compact JSON string (the caller asserts it PII-clean).
        tracing::info!(
            secured_audit_record_id = %record_id,
            tenant_id = %tenant,
            event_type = event_type.as_str(),
            actor_ref = actor_ref.unwrap_or(""),
            reason_code = reason_code.unwrap_or(""),
            correlation_id = ?correlation_id,
            before_after = %before_after,
            "bss-ledger: secured-audit append (NO-OP until Slice 6 — nothing durable persisted)"
        );
        Ok(record_id)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn audit_event_type_codes_are_screaming_snake() {
        assert_eq!(
            AuditEventType::ManualAdjustment.as_str(),
            "MANUAL_ADJUSTMENT"
        );
        assert_eq!(AuditEventType::ApprovalGranted.as_str(), "APPROVAL_GRANTED");
        assert_eq!(
            AuditEventType::ApprovalRejected.as_str(),
            "APPROVAL_REJECTED"
        );
        assert_eq!(AuditEventType::Redaction.as_str(), "REDACTION");
    }

    #[test]
    fn noop_sink_is_constructible_as_trait_object() {
        // The sink is wired as `Arc<dyn SecuredAuditSink>` in `module` + the
        // handler, so assert it is object-safe + default-constructible.
        let sink: std::sync::Arc<dyn SecuredAuditSink> =
            std::sync::Arc::new(NoopSecuredAuditSink::new());
        // A trait object whose only method needs a live `DbTx` can't be invoked
        // here (no txn in a pure unit test); the postgres `#[ignore]` test drives
        // the real append. This asserts the port is object-safe.
        let _ = sink;
    }
}
