"""Streaming transport scenarios (consumer/stream/1.01-1.14).

Skips:
- 1.07 and 1.08 are already covered by ``test_stream_guardrails.py``
- 1.09 (SSE happy path) is already covered by ``test_publish_consume.py``
- 1.06 (terminated subscription via shard shutdown) is xfail: triggering an
  internal shard teardown is not possible from outside in E2E.
- 1.14 (control-progress-frame) is xfail: requires a heavily-publishing producer
  and a narrow topic filter to make the frontier drift observable.

Patterns:
- Multipart (``/events:stream``) tests use ``MultipartReader`` from conftest.
- SSE tests use ``SseFrameReader`` from conftest.
- Every streaming test opens a fresh subscription on its own group/tenant_id.
"""

from __future__ import annotations

import asyncio
import uuid
from datetime import datetime, timezone

import httpx
import pytest

from .conftest import (
    EVENT_TYPE_STREAM,
    SUBJECT_TYPE,
    TOPIC_STREAM,
    TOPIC_4P,
    EVENT_TYPE_4P,
    MultipartReader,
    SseFrameReader,
)


def _now() -> str:
    return datetime.now(timezone.utc).isoformat()


async def _setup_seeded_sub(client, topic: str = TOPIC_STREAM, event_type: str = EVENT_TYPE_STREAM) -> tuple[str, str]:
    """Create group → JOIN → SEEK earliest.  Returns (tenant_id, sub_id)."""
    group_resp = await client.post("/consumer-groups")
    assert group_resp.status_code == 201
    group_id = group_resp.json()["id"]

    tenant_id = str(uuid.uuid4())
    sub_resp = await client.post(
        "/subscriptions",
        json={
            "consumer_group": group_id,
            "client_agent": "e2e-test",
            "interests": [{"topic": topic, "tenant_id": tenant_id, "types": [event_type]}],
        },
    )
    assert sub_resp.status_code == 201
    sub_id = str(sub_resp.json()["id"])
    assigned = sub_resp.json()["assigned"]

    positions = [
        {"topic": a["topic"], "partition": a["partition"], "value": "earliest"}
        for a in assigned
    ]
    seek_resp = await client.post(
        f"/subscriptions/{sub_id}:seek",
        json={"partition_positions": positions},
    )
    assert seek_resp.status_code == 200
    return tenant_id, sub_id


@pytest.mark.timeout(45, func_only=True)
async def test_multipart_stream_delivers_event_frame(api, test_env):
    """scenario: consumer/stream/1.01-positive-stream-multipart-frames.md

    GET /events:stream with ``Accept: multipart/mixed`` returns a multipart
    response.  The first frame is a ``topology`` frame, then events arrive as
    ``event`` frames.
    """
    async with api() as client:
        tenant_id, sub_id = await _setup_seeded_sub(client)

    event_id = str(uuid.uuid4())
    occurred_at = _now()
    async with httpx.AsyncClient(
        base_url=f"{test_env.base_url}/event-broker/v1", timeout=None
    ) as stream_client:
        async with stream_client.stream(
            "GET",
            f"/events:stream?subscription_id={sub_id}",
            headers={"Accept": "multipart/mixed"},
        ) as stream_resp:
            assert stream_resp.status_code == 200
            ct = stream_resp.headers.get("content-type", "")
            assert "multipart/mixed" in ct

            reader = MultipartReader(stream_resp)
            topology = await reader.await_kind("topology")
            assert "topology_version" in topology
            assert "assigned" in topology

            # Publish an event and verify it arrives as a multipart frame.
            async with api() as pub_client:
                pub_resp = await pub_client.post(
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
            assert pub_resp.status_code == 202

            event_frame = await reader.await_kind("event")

    payload = event_frame["payload"]
    assert event_frame == {
        "kind": "event",
        "payload": {
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
        },
    }


@pytest.mark.timeout(20, func_only=True)
async def test_heartbeat_frame_arrives_on_idle_stream(api, test_env):
    """scenario: consumer/stream/1.02-positive-stream-heartbeat-cadence.md

    An idle SSE stream emits a ``heartbeat`` frame approximately every 5 s.
    The test waits up to 12 s for one heartbeat frame after the topology frame.
    """
    async with api() as client:
        tenant_id, sub_id = await _setup_seeded_sub(client)

    async with httpx.AsyncClient(
        base_url=f"{test_env.base_url}/event-broker/v1", timeout=None
    ) as stream_client:
        async with stream_client.stream(
            "GET", f"/events:sse?subscription_id={sub_id}"
        ) as stream_resp:
            assert stream_resp.status_code == 200
            reader = SseFrameReader(stream_resp)
            kind, _ = await reader.next_frame(timeout=5)
            assert kind == "topology"

            # The next frame should be a heartbeat within ~10 s.
            kind, data = await reader.next_frame(timeout=12)
            assert kind == "heartbeat"
            assert "at" in data


@pytest.mark.timeout(20, func_only=True)
async def test_topology_frame_on_rebalance(api, test_env):
    """scenario: consumer/stream/1.03-positive-stream-topology-frame-on-rebalance.md

    A mid-stream JOIN by a second member triggers a topology frame on the first
    member's open stream.  Uses TOPIC_4P so there are enough partitions to
    split across two members.
    """
    tenant_id = str(uuid.uuid4())
    async with api() as client:
        group_resp = await client.post("/consumer-groups")
        assert group_resp.status_code == 201
        group_id = group_resp.json()["id"]

        sub1_resp = await client.post(
            "/subscriptions",
            json={
                "consumer_group": group_id,
                "client_agent": "e2e-test-1",
                "interests": [{"topic": TOPIC_4P, "tenant_id": tenant_id, "types": [EVENT_TYPE_4P]}],
            },
        )
        assert sub1_resp.status_code == 201
        sub1_id = str(sub1_resp.json()["id"])
        assigned = sub1_resp.json()["assigned"]

        seek_resp = await client.post(
            f"/subscriptions/{sub1_id}:seek",
            json={
                "partition_positions": [
                    {"topic": a["topic"], "partition": a["partition"], "value": "earliest"}
                    for a in assigned
                ]
            },
        )
        assert seek_resp.status_code == 200

    async with httpx.AsyncClient(
        base_url=f"{test_env.base_url}/event-broker/v1", timeout=None
    ) as stream_client:
        async with stream_client.stream(
            "GET", f"/events:sse?subscription_id={sub1_id}"
        ) as stream_resp:
            assert stream_resp.status_code == 200
            reader = SseFrameReader(stream_resp)

            # Initial topology: sub1 is the sole member and owns all 4
            # partitions. No events have been published to TOPIC_4P in this
            # test, so offset and last_examined are both 0.
            kind, init_data = await reader.next_frame(timeout=5)
            assert kind == "topology"
            assert init_data["topology_version"] == 1
            assert sorted(init_data["assigned"], key=lambda a: a["partition"]) == [
                {"topic": TOPIC_4P, "partition": p, "offset": 0, "last_examined": 0}
                for p in range(4)
            ]

            # Second member JOINs → triggers rebalance → topology frame.
            async with api() as client2:
                sub2_resp = await client2.post(
                    "/subscriptions",
                    json={
                        "consumer_group": group_id,
                        "client_agent": "e2e-test-2",
                        "interests": [{"topic": TOPIC_4P, "tenant_id": tenant_id, "types": [EVENT_TYPE_4P]}],
                    },
                )
            assert sub2_resp.status_code == 201

            # Sub1's stream receives a topology frame reflecting the 2/2
            # partition split. topology_version advances to 2 and exactly 2
            # of the original 4 partitions remain assigned to sub1. Cursors
            # for the retained partitions are unchanged (offset = 0, since no
            # events were delivered before the rebalance).
            kind, rebalance_data = await reader.next_frame(timeout=10)

    assert kind == "topology"
    assert rebalance_data["topology_version"] == 2
    rebalanced = sorted(rebalance_data["assigned"], key=lambda a: a["partition"])
    assert len(rebalanced) == 2
    assert {a["partition"] for a in rebalanced} <= {0, 1, 2, 3}
    assert all(
        a == {"topic": TOPIC_4P, "partition": a["partition"], "offset": 0, "last_examined": 0}
        for a in rebalanced
    )


async def test_positions_not_set_returns_409(api):
    """scenario: consumer/stream/1.04-negative-stream-positions-not-set.md

    Opening a stream without prior SEEK returns 409 PositionsNotSet.
    The error.rs override sets HTTP 409 for this condition.
    """
    tenant_id = str(uuid.uuid4())
    async with api() as client:
        group_resp = await client.post("/consumer-groups")
        assert group_resp.status_code == 201
        group_id = group_resp.json()["id"]

        sub_resp = await client.post(
            "/subscriptions",
            json={
                "consumer_group": group_id,
                "client_agent": "e2e-test",
                "interests": [{"topic": TOPIC_STREAM, "tenant_id": tenant_id, "types": [EVENT_TYPE_STREAM]}],
            },
        )
        assert sub_resp.status_code == 201
        sub_id = str(sub_resp.json()["id"])

        # No SEEK performed — stream must reject.
        stream_resp = await client.get(
            f"/events:stream?subscription_id={sub_id}",
            headers={"Accept": "multipart/mixed"},
        )
    assert stream_resp.status_code == 409
    body = stream_resp.json()
    assert body == {
        "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.failed_precondition.v1~",
        "title": "Failed Precondition",
        "status": 409,
        "detail": body["detail"],
        "instance": body["instance"],
        "trace_id": body["trace_id"],
        "context": {
            "resource_type": "gts.cf.core.events.subscription.v1~",
            "violations": [
                {
                    "type": "cursor_missing",
                    "subject": f"{TOPIC_STREAM}:0",
                    "description": body["context"]["violations"][0]["description"],
                }
            ]
        },
    }


async def test_unknown_subscription_returns_404(api):
    """scenario: consumer/stream/1.05-negative-stream-unknown-subscription.md"""
    fake_sub_id = str(uuid.uuid4())
    async with api() as client:
        resp = await client.get(
            f"/events:stream?subscription_id={fake_sub_id}",
            headers={"Accept": "multipart/mixed"},
        )
    assert resp.status_code == 404
    body = resp.json()
    assert body == {
        "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.not_found.v1~",
        "title": "Not Found",
        "status": 404,
        "detail": body["detail"],
        "instance": body["instance"],
        "trace_id": body["trace_id"],
        "context": {
            "resource_type": "gts.cf.core.events.subscription.v1~",
            "resource_name": fake_sub_id,
        },
    }


@pytest.mark.xfail(
    reason="triggering a terminated subscription requires internal shard shutdown not available in E2E",
    strict=False,
)
async def test_terminated_subscription_returns_410(api):
    """scenario: consumer/stream/1.06-negative-stream-terminated-subscription.md

    When the delivery shard shuts down it closes the stream with 410.  Not
    triggerable from the outside in E2E.
    """
    async with api() as client:
        _, sub_id = await _setup_seeded_sub(client)
    async with httpx.AsyncClient(timeout=None) as stream_client:
        async with stream_client.stream(
            "GET", f"/events:sse?subscription_id={sub_id}"
        ) as stream_resp:
            assert stream_resp.status_code == 410


# 1.07 and 1.08 are covered by test_stream_guardrails.py — not duplicated here.
# 1.09 (SSE happy path) is covered by test_publish_consume.py.


async def test_stream_rejects_unknown_query_params(api):
    """scenario: consumer/stream/1.10-negative-stream-rejects-timeout-collect-params.md

    Legacy ``timeout`` and ``collect`` query parameters are not accepted;
    the endpoint only recognises ``subscription_id``.
    """
    fake_sub_id = str(uuid.uuid4())
    async with api() as client:
        resp = await client.get(
            f"/events:stream?subscription_id={fake_sub_id}&timeout=20&collect=50",
            headers={"Accept": "multipart/mixed"},
        )
    # Either 400 (unknown params) or 404 (sub not found) is acceptable; this
    # scenario specifically requires 400 for the unknown-param rejection.
    assert resp.status_code == 400
    body = resp.json()
    assert body == {
        "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~",
        "title": "Invalid Argument",
        "status": 400,
        "detail": body["detail"],
        "instance": body["instance"],
        "trace_id": body["trace_id"],
        "context": body["context"],
    }


@pytest.mark.timeout(15, func_only=True)
async def test_second_stream_on_same_subscription_returns_409(api, test_env):
    """scenario: consumer/stream/1.11-negative-streaming-in-progress.md

    A second concurrent stream on the same ``subscription_id`` is rejected 409.
    """
    async with api() as client:
        _, sub_id = await _setup_seeded_sub(client)

    async with httpx.AsyncClient(
        base_url=f"{test_env.base_url}/event-broker/v1", timeout=None
    ) as stream_client:
        async with stream_client.stream(
            "GET", f"/events:sse?subscription_id={sub_id}"
        ) as stream_resp:
            assert stream_resp.status_code == 200

            async with api() as second_client:
                second_resp = await second_client.get(
                    f"/events:sse?subscription_id={sub_id}",
                )

    assert second_resp.status_code == 409
    body = second_resp.json()
    assert body == {
        "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.failed_precondition.v1~",
        "title": "Failed Precondition",
        "status": 409,
        "detail": body["detail"],
        "instance": body["instance"],
        "trace_id": body["trace_id"],
        "context": {
            "resource_type": "gts.cf.core.events.subscription.v1~",
            "violations": [
                {
                    "type": "streaming_in_progress",
                    "subject": sub_id,
                    "description": body["context"]["violations"][0]["description"],
                }
            ]
        },
    }


@pytest.mark.timeout(15, func_only=True)
async def test_delete_while_streaming_closes_connection(api, test_env):
    """scenario: consumer/stream/1.13-positive-delete-while-streaming.md

    DELETE /subscriptions/{id} while a stream is open terminates the stream
    and returns 204.  No control frame precedes the close.
    """
    async with api() as client:
        _, sub_id = await _setup_seeded_sub(client)

    async with httpx.AsyncClient(
        base_url=f"{test_env.base_url}/event-broker/v1", timeout=None
    ) as stream_client:
        async with stream_client.stream(
            "GET", f"/events:sse?subscription_id={sub_id}"
        ) as stream_resp:
            assert stream_resp.status_code == 200

            # Consume the topology frame.
            reader = SseFrameReader(stream_resp)
            kind, _ = await reader.next_frame(timeout=5)
            assert kind == "topology"

            # DELETE while the stream is open.
            async with api() as del_client:
                del_resp = await del_client.delete(f"/subscriptions/{sub_id}")
            assert del_resp.status_code == 204

    # After DELETE, the subscription is gone.
    async with api() as check_client:
        get_resp = await check_client.get(f"/subscriptions/{sub_id}")
    assert get_resp.status_code == 404


@pytest.mark.timeout(30, func_only=True)
async def test_stream_terminates_with_control_on_rebalance_gain(api, test_env):
    """scenario: consumer/stream/1.12-positive-stream-terminates-on-rebalance-gain.md

    When a group member leaves and the surviving member *gains* partitions, the
    broker terminates the surviving member's open subscription with a
    ``control`` frame with ``code: "terminal"``.  Partition loss emits a
    ``topology`` frame; partition gain emits a terminal ``control`` frame.
    """
    tenant_id = str(uuid.uuid4())
    base_url = f"{test_env.base_url}/event-broker/v1"

    async with api() as client:
        group_resp = await client.post("/consumer-groups")
        assert group_resp.status_code == 201
        group_id = group_resp.json()["id"]

        # Member A: joins first, gets all 4 partitions.
        sub_a_resp = await client.post(
            "/subscriptions",
            json={
                "consumer_group": group_id,
                "client_agent": "consumer-a",
                "interests": [{"topic": TOPIC_4P, "tenant_id": tenant_id, "types": [EVENT_TYPE_4P]}],
            },
        )
        assert sub_a_resp.status_code == 201
        sub_a_id = str(sub_a_resp.json()["id"])
        assigned_a = sub_a_resp.json()["assigned"]

        seek_a = await client.post(
            f"/subscriptions/{sub_a_id}:seek",
            json={
                "partition_positions": [
                    {"topic": p["topic"], "partition": p["partition"], "value": "earliest"}
                    for p in assigned_a
                ]
            },
        )
        assert seek_a.status_code == 200

        # Member B: joins second, triggers rebalance (2+2 split).
        sub_b_resp = await client.post(
            "/subscriptions",
            json={
                "consumer_group": group_id,
                "client_agent": "consumer-b",
                "interests": [{"topic": TOPIC_4P, "tenant_id": tenant_id, "types": [EVENT_TYPE_4P]}],
            },
        )
        assert sub_b_resp.status_code == 201
        sub_b_id = str(sub_b_resp.json()["id"])
        assigned_b = sub_b_resp.json()["assigned"]

        # After rebalance, A now holds 2 partitions.  SEEK A's new (post-split) assignment.
        re_seek_a = await client.post(
            f"/subscriptions/{sub_a_id}:seek",
            json={
                "partition_positions": [
                    {"topic": p["topic"], "partition": p["partition"], "value": "earliest"}
                    for p in sub_a_resp.json()["assigned"]
                ]
            },
        )
        assert re_seek_a.status_code == 200

        seek_b = await client.post(
            f"/subscriptions/{sub_b_id}:seek",
            json={
                "partition_positions": [
                    {"topic": p["topic"], "partition": p["partition"], "value": "earliest"}
                    for p in assigned_b
                ]
            },
        )
        assert seek_b.status_code == 200

    # Open A's stream.
    async with httpx.AsyncClient(base_url=base_url, timeout=None) as stream_a:
        async with stream_a.stream("GET", f"/events:sse?subscription_id={sub_a_id}") as resp_a:
            assert resp_a.status_code == 200
            reader_a = SseFrameReader(resp_a)

            # Consume the initial topology.
            kind, _ = await reader_a.next_frame(timeout=5)
            assert kind == "topology"

            # B leaves → A gains partitions → broker terminates A's subscription.
            async with api() as del_client:
                del_resp = await del_client.delete(f"/subscriptions/{sub_b_id}")
            assert del_resp.status_code == 204

            # Gain emits a control/terminal frame (not a topology frame).
            while True:
                kind, ctrl = await reader_a.next_frame(timeout=10)
                if kind != "heartbeat":
                    break
            assert kind == "control"
            assert ctrl["code"] == "terminal"


@pytest.mark.xfail(
    reason=(
        "control-progress-frame requires a narrow topic filter that rejects most events "
        "while a producer publishes heavily — not achievable with the standard E2E topics"
    ),
    strict=False,
)
@pytest.mark.timeout(30, func_only=True)
async def test_control_progress_frame_emitted_on_filtered_stream(api, test_env):
    """scenario: consumer/stream/1.14-positive-control-progress-frame.md

    When a consumer's filter rejects most events, the broker emits a
    ``control`` frame with ``code: "progress"`` carrying the frontier drift.
    Not triggerable in E2E with standard topic configuration.
    """
    async with api() as client:
        _, sub_id = await _setup_seeded_sub(client)

    async with httpx.AsyncClient(
        base_url=f"{test_env.base_url}/event-broker/v1", timeout=None
    ) as stream_client:
        async with stream_client.stream(
            "GET", f"/events:sse?subscription_id={sub_id}"
        ) as stream_resp:
            assert stream_resp.status_code == 200
            reader = SseFrameReader(stream_resp)
            _, _ = await reader.next_frame(timeout=5)  # topology

            # Expect a control/progress frame — requires heavy publish + narrow filter.
            kind, data = await reader.next_frame(timeout=25)
            assert kind == "control"
            assert data["code"] == "progress"
            assert "positions" in data
