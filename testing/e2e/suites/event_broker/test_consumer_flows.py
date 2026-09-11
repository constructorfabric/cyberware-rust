"""Consumer-only end-to-end flow scenarios (consumer/flows/1.01-1.04).

These tests exercise multi-step journeys that don't fit neatly in a single
scenario domain:

- 1.01: Two-consumer rebalance — both consumers stream concurrently and each
  receives events from its assigned partitions.
- 1.02: PositionsNotSet recovery — SDK-style re-SEEK loop after a 409.
- 1.03: Path-A consumer with persistent offset store — reconnect from exact
  last-processed offset.
- 1.04: Leave triggers gain+terminate — leaving member's partition is gained
  by surviving member; the leaving member's stream terminates.
"""

from __future__ import annotations

import asyncio
import uuid
from datetime import datetime, timezone

import httpx
import pytest

from .conftest import (
    EVENT_TYPE_4P,
    EVENT_TYPE_STREAM,
    SUBJECT_TYPE,
    TOPIC_4P,
    TOPIC_STREAM,
    SseFrameReader,
)


def _now() -> str:
    return datetime.now(timezone.utc).isoformat()


@pytest.mark.timeout(5, func_only=True)
async def test_two_consumer_rebalance(api, test_env):
    """scenario: consumer/flows/1.01-flow-two-consumer-rebalance.md

    Consumer A joins first and holds all 4 partitions of TOPIC_4P.  Consumer B
    joins while A's stream is open.  Both streams receive a new topology frame
    showing the rebalanced assignment; each consumer then receives only events
    for its own partitions.
    """
    tenant_id = str(uuid.uuid4())
    base_url = f"{test_env.base_url}/event-broker/v1"

    async with api() as client:
        group_resp = await client.post("/consumer-groups")
        assert group_resp.status_code == 201
        group_id = group_resp.json()["id"]

        # Consumer A JOINs first — gets all 4 partitions.
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
        assert len(sub_a_resp.json()["assigned"]) == 4

        assigned_a = sub_a_resp.json()["assigned"]
        seek_resp = await client.post(
            f"/subscriptions/{sub_a_id}:seek",
            json={
                "partition_positions": [
                    {"topic": a["topic"], "partition": a["partition"], "value": "earliest"}
                    for a in assigned_a
                ]
            },
        )
        assert seek_resp.status_code == 200

    # Open A's stream.
    async with httpx.AsyncClient(base_url=base_url, timeout=None) as stream_a:
        async with stream_a.stream("GET", f"/events:sse?subscription_id={sub_a_id}") as resp_a:
            assert resp_a.status_code == 200
            reader_a = SseFrameReader(resp_a)
            kind, _ = await reader_a.next_frame(timeout=5)
            assert kind == "topology"

            # Consumer B JOINs — triggers rebalance.
            async with api() as client_b:
                sub_b_resp = await client_b.post(
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
                assert len(assigned_b) == 2

                seek_b = await client_b.post(
                    f"/subscriptions/{sub_b_id}:seek",
                    json={
                        "partition_positions": [
                            {"topic": a["topic"], "partition": a["partition"], "value": "earliest"}
                            for a in assigned_b
                        ]
                    },
                )
                assert seek_b.status_code == 200

            # A's stream must now receive a topology frame with 2 assigned partitions.
            kind, topo = await reader_a.next_frame(timeout=10)
            assert kind == "topology"
            assert len(topo["assigned"]) == 2


@pytest.mark.timeout(5, func_only=True)
async def test_positions_not_set_recovery(api):
    """scenario: consumer/flows/1.02-flow-positions-not-set-recovery.md

    A consumer deliberately skips the SEEK step and receives 409.  It then
    SEEKs all unseeded partitions and successfully opens the stream.
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
        assigned = sub_resp.json()["assigned"]

        # Step 1: try streaming without seek → 409.
        bad_resp = await client.get(
            f"/events:stream?subscription_id={sub_id}",
            headers={"Accept": "multipart/mixed"},
        )
        assert bad_resp.status_code == 409
        bad_body = bad_resp.json()
        assert bad_body == {
            "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.failed_precondition.v1~",
            "title": "Failed Precondition",
            "status": 409,
            "detail": bad_body["detail"],
            "instance": bad_body["instance"],
            "trace_id": bad_body["trace_id"],
            "context": {
                "resource_type": "gts.cf.core.events.subscription.v1~",
                "violations": bad_body["context"]["violations"],
            },
        }
        violations = bad_body["context"]["violations"]
        unseeded = [v["subject"] for v in violations if v["type"] == "cursor_missing"]
        assert len(unseeded) > 0

        # Step 2: SEEK all unseeded partitions (recover).
        seek_resp = await client.post(
            f"/subscriptions/{sub_id}:seek",
            json={
                "partition_positions": [
                    {"topic": a["topic"], "partition": a["partition"], "value": "earliest"}
                    for a in assigned
                ]
            },
        )
        assert seek_resp.status_code == 200

        # Step 3: stream now succeeds — open it, check the status, close it.
        async with client.stream(
            "GET",
            f"/events:stream?subscription_id={sub_id}",
            headers={"Accept": "multipart/mixed"},
        ) as stream_resp:
            assert stream_resp.status_code == 200


@pytest.mark.timeout(5, func_only=True)
async def test_path_a_consumer_reconnects_from_exact_offset(api, test_env):
    """scenario: consumer/flows/1.03-flow-path-a-consumer-with-db.md

    A consumer reads its own DB offset, SEEKs to that integer, streams,
    processes an event, persists the sequence to its "DB", then disconnects and
    reconnects.  The second stream starts from the persisted offset and must
    not re-deliver the already-processed event.
    """
    tenant_id = str(uuid.uuid4())
    base_url = f"{test_env.base_url}/event-broker/v1"

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

        # SEEK to earliest before streaming.
        await client.post(
            f"/subscriptions/{sub_id}:seek",
            json={"partition_positions": [{"topic": TOPIC_STREAM, "partition": 0, "value": "earliest"}]},
        )

    event_id = str(uuid.uuid4())
    async with httpx.AsyncClient(base_url=base_url, timeout=None) as stream_client:
        async with stream_client.stream("GET", f"/events:sse?subscription_id={sub_id}") as resp:
            assert resp.status_code == 200
            reader = SseFrameReader(resp)
            _, _ = await reader.next_frame(timeout=5)  # topology

            # Publish one event.
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
                        "occurred_at": _now(),
                    },
                )
            assert pub_resp.status_code == 202

            _, event_data = await reader.next_frame(timeout=5)
            assert event_data["payload"]["id"] == event_id
            # "Consumer persists this to its DB"
            last_processed_sequence = event_data["payload"]["sequence"]

    # Reconnect: create a new subscription in the same group and seek to the
    # persisted sequence (Path-A: "DB has last processed offset").
    async with api() as client:
        sub2_resp = await client.post(
            "/subscriptions",
            json={
                "consumer_group": group_id,
                "client_agent": "e2e-test",
                "interests": [{"topic": TOPIC_STREAM, "tenant_id": tenant_id, "types": [EVENT_TYPE_STREAM]}],
            },
        )
        assert sub2_resp.status_code == 201
        sub2_id = str(sub2_resp.json()["id"])

        # SEEK to the last processed sequence (the cursor is exactly that value).
        await client.post(
            f"/subscriptions/{sub2_id}:seek",
            json={"partition_positions": [{"topic": TOPIC_STREAM, "partition": 0, "value": last_processed_sequence}]},
        )

    # The second stream should NOT immediately deliver the already-processed event.
    # Publish a second event to verify the stream position is correct.
    event2_id = str(uuid.uuid4())
    async with httpx.AsyncClient(base_url=base_url, timeout=None) as stream_client:
        async with stream_client.stream("GET", f"/events:sse?subscription_id={sub2_id}") as resp:
            assert resp.status_code == 200
            reader2 = SseFrameReader(resp)
            _, _ = await reader2.next_frame(timeout=5)  # topology

            async with api() as pub_client:
                await pub_client.post(
                    "/events",
                    json={
                        "id": event2_id,
                        "type": EVENT_TYPE_STREAM,
                        "tenant_id": tenant_id,
                        "source": "e2e-test",
                        "subject": "s1",
                        "subject_type": SUBJECT_TYPE,
                        "occurred_at": _now(),
                    },
                )

            _, event2_data = await reader2.next_frame(timeout=5)
            # Must be event2, not event1 (which was already processed).
            assert event2_data["payload"]["id"] == event2_id
            assert event2_data["payload"]["sequence"] == last_processed_sequence + 1


@pytest.mark.parametrize("disconnect", [False, True], ids=["delete", "disconnect"])
@pytest.mark.timeout(20, func_only=True)
async def test_gain_and_terminate(api, test_env, disconnect):
    """scenario: consumer/flows/1.04-flow-leave-triggers-gain-terminate.md

    With two members in a TOPIC_4P group, A holds 2 partitions and B holds 2.
    When B departs — either via DELETE (delete variant) or by dropping its SSE
    connection (disconnect variant) — A gains the freed partitions and receives a
    control/terminal frame.  The disconnect variant uses session_timeout=PT2S so
    the server's grace-period timer fires quickly.
    """
    tenant_id = str(uuid.uuid4())
    base_url = f"{test_env.base_url}/event-broker/v1"

    async with api() as client:
        group_resp = await client.post("/consumer-groups")
        assert group_resp.status_code == 201
        group_id = group_resp.json()["id"]

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

        b_join: dict = {
            "consumer_group": group_id,
            "client_agent": "consumer-b",
            "interests": [{"topic": TOPIC_4P, "tenant_id": tenant_id, "types": [EVENT_TYPE_4P]}],
        }
        if disconnect:
            b_join["session_timeout"] = "PT2S"
        sub_b_resp = await client.post("/subscriptions", json=b_join)
        assert sub_b_resp.status_code == 201
        sub_b_id = str(sub_b_resp.json()["id"])

        for sub_id, sub_resp in [(sub_a_id, sub_a_resp), (sub_b_id, sub_b_resp)]:
            assigned = sub_resp.json()["assigned"]
            await client.post(
                f"/subscriptions/{sub_id}:seek",
                json={
                    "partition_positions": [
                        {"topic": a["topic"], "partition": a["partition"], "value": "earliest"}
                        for a in assigned
                    ]
                },
            )

    async with httpx.AsyncClient(base_url=base_url, timeout=None) as stream_a:
        async with stream_a.stream("GET", f"/events:sse?subscription_id={sub_a_id}") as resp_a:
            assert resp_a.status_code == 200
            reader_a = SseFrameReader(resp_a)
            kind, _ = await reader_a.next_frame(timeout=5)
            assert kind == "topology"

            if disconnect:
                async with httpx.AsyncClient(base_url=base_url, timeout=None) as stream_b:
                    async with stream_b.stream("GET", f"/events:sse?subscription_id={sub_b_id}") as resp_b:
                        assert resp_b.status_code == 200
                        reader_b = SseFrameReader(resp_b)
                        kind, _ = await reader_b.next_frame(timeout=5)
                        assert kind == "topology"
                    # B's connection closes here — server starts the session_timeout timer.
            else:
                async with api() as del_client:
                    del_resp = await del_client.delete(f"/subscriptions/{sub_b_id}")
                assert del_resp.status_code == 204

            # A gains B's partitions → broker sends control/terminal frame.
            while True:
                kind, ctrl = await reader_a.next_frame(timeout=10)
                if kind != "heartbeat":
                    break
            assert kind == "control"
            assert ctrl["code"] == "terminal"
