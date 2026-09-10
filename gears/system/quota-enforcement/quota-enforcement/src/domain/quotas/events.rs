//! The `quota-changed` notification event the lifecycle enqueues with every
//! mutation (invariant I11; PRD section 5.15 event catalogue).
//!
//! The gear builds the event and hands it to the storage primitive, which
//! enqueues it in the mutation's transaction. Dispatch belongs to the
//! notifications feature.

use quota_enforcement_sdk::{
    EventId, NotificationEvent, NotificationEventKind, QuotaId, SubjectRef, TenantId,
};
use serde_json::json;
use time::OffsetDateTime;
use toolkit_macros::domain_model;

/// The `change_kind` discriminator of a `quota-changed` event.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// A Quota was created.
    Created,
    /// A Quota was updated without changing its identity.
    Updated,
    /// A Quota was deactivated.
    Deactivated,
}

impl ChangeKind {
    /// The wire discriminator.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Updated => "updated",
            Self::Deactivated => "deactivated",
        }
    }
}

/// A `quota-changed` event. `quota_id` is `None` on create, where storage
/// assigns the id and fills it in.
#[must_use]
pub fn quota_changed(
    tenant_id: TenantId,
    quota_id: Option<QuotaId>,
    subject: Option<SubjectRef>,
    kind: ChangeKind,
    now: OffsetDateTime,
) -> NotificationEvent {
    NotificationEvent {
        event_id: EventId::generate(),
        kind: NotificationEventKind::QuotaChanged,
        tenant_id,
        quota_id,
        policy_id: None,
        subject,
        payload: json!({ "change_kind": kind.as_str() }),
        emitted_at: now,
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "events_tests.rs"]
mod events_tests;
