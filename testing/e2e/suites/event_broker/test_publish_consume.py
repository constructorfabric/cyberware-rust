"""Publish -> consume e2e happy path (design.md D8, task 11.2).

Every request body sent and response body asserted is inlined per test -
no shared helper hides a request shape (unlike `gears/oagw/helpers.py`'s
`create_upstream`/`create_route`, which build the JSON body internally).
"""

from __future__ import annotations

import uuid
from datetime import datetime, timezone

import httpx

from .conftest import EVENT_TYPE_STREAM, SUBJECT_TYPE, TOPIC_STREAM, SseFrameReader


async def test_publish_then_consume_happy_path(api, test_env):
    """scenario: consumer/stream/1.09-positive-sse-event-stream.md"""
    async with api() as client:
        group_resp = await client.post("/consumer-groups")
        assert group_resp.status_code == 201
        group_id = group_resp.json()["id"]

        tenant_id = str(uuid.uuid4())
        sub_resp = await client.post(
            "/subscriptions",
            json={
                "consumer_group": group_id,
                "client_agent": "e2e-test",
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
        sub_id = sub_resp.json()["id"]

        seek_resp = await client.post(
            f"/subscriptions/{sub_id}:seek",
            json={
                "partition_positions": [
                    {"topic": TOPIC_STREAM, "partition": 0, "value": "earliest"}
                ]
            },
        )
        assert seek_resp.status_code == 200

    async with httpx.AsyncClient(
        base_url=f"{test_env.base_url}/event-broker/v1", timeout=None
    ) as stream_client:
        async with stream_client.stream(
            "GET", f"/events:sse?subscription_id={sub_id}"
        ) as stream_resp:
            assert stream_resp.status_code == 200
            reader = SseFrameReader(stream_resp)
            topology_kind, topology_data = await reader.next_frame(timeout=5)
            assert topology_kind == "topology"
            assert topology_data == {
                "kind": "topology",
                # One join has happened, so the group is at version 1, not 0:
                # the version increments on every membership change, and
                # `scenarios/consumer/stream/1.03` has a second joiner take it
                # from 1 to 2.
                "topology_version": 1,
                "assigned": [
                    {"topic": TOPIC_STREAM, "partition": 0, "offset": 0, "last_examined": 0}
                ],
            }

            event_id = str(uuid.uuid4())
            occurred_at = datetime.now(timezone.utc).isoformat()
            async with api() as publish_client:
                publish_resp = await publish_client.post(
                    "/events",
                    json={
                        "id": event_id,
                        "type": EVENT_TYPE_STREAM,
                        "tenant_id": tenant_id,
                        "source": "e2e-test",
                        "subject": "s1",
                        "subject_type": SUBJECT_TYPE,
                        "occurred_at": occurred_at,
                    },
                )
            assert publish_resp.status_code == 202
            assert publish_resp.text == "", "202 Accepted must carry no body"

            event_kind, event_data = await reader.next_frame(timeout=5)
            assert event_kind == "event"
            # `occurred_at`/`sequence_time` are round-tripped through the
            # server rather than assumed byte-identical to what was sent -
            # `occurred_at` normalizes to a different (but equivalent)
            # RFC 3339 rendering, and `sequence_time` is server-generated
            # with no caller-supplied value to compare against at all.
            #
            # `sequence` is server-assigned per `(topic, partition)`, and the
            # partition is derived by hashing `tenant_id`. TOPIC_STREAM has one
            # partition, so every test publishing here shares a single counter
            # and no test can predict its own absolute value - a unique
            # `tenant_id` isolates which events are delivered, not how they are
            # numbered.
            payload = event_data["payload"]
            assert payload == {
                "id": event_id,
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
