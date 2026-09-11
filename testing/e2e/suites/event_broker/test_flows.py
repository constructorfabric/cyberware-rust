"""Coupled producer + consumer journey (flows/1.01).

Composes all integration steps into one transcript:
1. Producer publishes 3 events.
2. Consumer creates a group, JOINs, SEEKs to earliest.
3. Consumer opens an SSE stream and receives all 3 events in order.

This test exercises the full publish→subscribe→consume pipeline and confirms
that events published before the subscription is opened are replayed correctly
(the broker replays from the cursor set by SEEK, not from the subscription's
join time).
"""

from __future__ import annotations

import uuid
from datetime import datetime, timezone

import httpx
import pytest

from .conftest import EVENT_TYPE_STREAM, SUBJECT_TYPE, TOPIC_STREAM, SseFrameReader


@pytest.mark.timeout(30, func_only=True)
async def test_publish_subscribe_consume_full_flow(api, test_env):
    """scenario: flows/1.01-flow-publish-subscribe-consume.md"""
    tenant_id = str(uuid.uuid4())
    occurred_at = datetime.now(timezone.utc).isoformat()

    # Step 1 — Publish 3 events BEFORE the consumer JOINs.
    event_ids = [str(uuid.uuid4()) for _ in range(3)]
    async with api() as pub_client:
        for eid in event_ids:
            pub_resp = await pub_client.post(
                "/events",
                json={
                    "id": eid,
                    "type": EVENT_TYPE_STREAM,
                    "tenant_id": tenant_id,
                    "source": "e2e-test",
                    "subject": "s1",
                    "subject_type": SUBJECT_TYPE,
                    "occurred_at": occurred_at,
                },
            )
            assert pub_resp.status_code == 202

    # Step 2 — Consumer creates a group, JOINs, SEEKs to earliest.
    async with api() as client:
        group_resp = await client.post("/consumer-groups")
        assert group_resp.status_code == 201
        group_id = group_resp.json()["id"]

        sub_resp = await client.post(
            "/subscriptions",
            json={
                "consumer_group": group_id,
                "client_agent": "e2e-flow-consumer",
                "interests": [
                    {
                        "topic": TOPIC_STREAM,
                        "tenant_id": tenant_id,
                        "types": [EVENT_TYPE_STREAM],
                    }
                ],
            },
        )
        assert sub_resp.status_code == 201
        sub_id = str(sub_resp.json()["id"])

        seek_resp = await client.post(
            f"/subscriptions/{sub_id}:seek",
            json={
                "partition_positions": [
                    {"topic": TOPIC_STREAM, "partition": 0, "value": "earliest"}
                ]
            },
        )
        assert seek_resp.status_code == 200

    # Step 3 — Stream and receive all 3 events.
    received_ids = []
    async with httpx.AsyncClient(
        base_url=f"{test_env.base_url}/event-broker/v1", timeout=None
    ) as stream_client:
        async with stream_client.stream(
            "GET", f"/events:sse?subscription_id={sub_id}"
        ) as stream_resp:
            assert stream_resp.status_code == 200
            reader = SseFrameReader(stream_resp)

            # First frame is always topology.
            kind, _ = await reader.next_frame(timeout=5)
            assert kind == "topology"

            # Read until we have all 3 events (or timeout).
            for _ in range(3):
                kind, data = await reader.next_frame(timeout=5)
                assert kind == "event"
                received_ids.append(data["payload"]["id"])
                payload = data["payload"]
                assert payload == {
                    "id": payload["id"],
                    "type": EVENT_TYPE_STREAM,
                    "topic": TOPIC_STREAM,
                    "tenant_id": tenant_id,
                    "source": "e2e-test",
                    "subject": "s1",
                    "subject_type": SUBJECT_TYPE,
                    "occurred_at": payload["occurred_at"],
                    "trace_parent": None,
                    "data": None,
                    "partition": 0,
                    "sequence": payload["sequence"],
                    "sequence_time": payload["sequence_time"],
                }

    # All 3 events arrived; order within a partition is guaranteed monotonic.
    assert set(received_ids) == set(event_ids)
    assert received_ids == sorted(received_ids, key=lambda eid: event_ids.index(eid)), (
        "events on a single partition must arrive in publish order"
    )
