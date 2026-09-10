#![allow(clippy::expect_used)]

use std::sync::Arc;

use quota_enforcement_sdk::{NotificationEvent, TenantId};
use uuid::Uuid;

use super::{AlreadyBound, EnqueueError, NOTIFICATION_PARTITIONS, NotificationEnqueuer, QeOutbox};
use crate::test_support::{bound_outbox, enqueued_messages, quota_changed, tenant, test_db};

#[test]
fn a_tenant_always_lands_on_the_same_partition_within_the_bound() {
    for raw in [0_u128, 1, 7, 8, u128::MAX, 0x00ac_ce55] {
        let tenant = TenantId::new(Uuid::from_u128(raw));
        let partition = QeOutbox::partition_for(tenant);
        assert!(partition < u32::from(NOTIFICATION_PARTITIONS), "{raw}");
        assert_eq!(partition, QeOutbox::partition_for(tenant), "stable");
    }
}

#[tokio::test]
async fn an_unbound_handle_refuses_every_enqueue_and_binds_once() {
    let db = test_db().await;
    let outbox = QeOutbox::new();
    assert!(!outbox.is_bound());
    let conn = db.conn().expect("connection");
    let err = outbox
        .enqueue_all(&conn, &[])
        .await
        .expect_err("nothing bound");
    assert!(matches!(err, EnqueueError::NotBound), "{err:?}");

    let (handle, bound) = bound_outbox(&db).await;
    assert!(bound.is_bound());
    assert_eq!(bound.bind(Arc::clone(handle.outbox())), Err(AlreadyBound));
    handle.stop().await;
}

#[tokio::test]
async fn events_are_enqueued_in_order_with_their_kind_as_the_payload_type() {
    let db = test_db().await;
    let (handle, outbox) = bound_outbox(&db).await;
    let first = quota_changed(tenant());
    let mut second = quota_changed(tenant());
    second.payload = serde_json::json!({ "change_kind": "updated" });
    {
        let conn = db.conn().expect("connection");
        let ids = outbox
            .enqueue_all(&conn, &[first.clone(), second.clone()])
            .await
            .expect("enqueued");
        assert_eq!(ids.len(), 2);
        assert!(
            outbox
                .enqueue_all(&conn, &[])
                .await
                .expect("nothing to do")
                .is_empty()
        );
    }

    let messages = enqueued_messages(&db).await;
    assert_eq!(messages.len(), 2);
    for (message, event) in messages.iter().zip([&first, &second]) {
        assert_eq!(message.payload_type, "quota-changed");
        let decoded: NotificationEvent =
            serde_json::from_slice(&message.payload).expect("event json");
        assert_eq!(&decoded, event);
    }
    handle.stop().await;
}
