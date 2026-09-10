#![allow(clippy::expect_used)]

use quota_enforcement_sdk::{NotificationEventKind, QuotaId};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use super::{ChangeKind, quota_changed};
use crate::test_support::tenant;

#[test]
fn quota_changed_carries_the_kind_the_target_and_the_discriminator() {
    let now = OffsetDateTime::from_unix_timestamp(1_000).expect("timestamp");
    let id = QuotaId::new(Uuid::from_u128(9));
    let event = quota_changed(tenant(), Some(id), None, ChangeKind::Updated, now);
    assert_eq!(event.kind, NotificationEventKind::QuotaChanged);
    assert_eq!(event.tenant_id, tenant());
    assert_eq!(event.quota_id, Some(id));
    assert_eq!(event.policy_id, None);
    assert_eq!(event.payload, json!({ "change_kind": "updated" }));
    assert_eq!(event.emitted_at, now);

    let created = quota_changed(tenant(), None, None, ChangeKind::Created, now);
    assert_eq!(created.quota_id, None, "storage fills the id it assigns");
    assert_eq!(created.payload["change_kind"], json!("created"));
    assert_ne!(created.event_id, event.event_id, "every event is unique");
    assert_eq!(ChangeKind::Deactivated.as_str(), "deactivated");
}
