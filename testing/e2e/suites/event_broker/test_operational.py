"""Operational tests - properties of the running broker that no scenario defines.

Every other file in this suite asserts the documented HTTP contract, and every
test in them names the scenario it covers in its docstring. The tests here have
no scenario on purpose: they assert operational properties - delivery latency,
survival across a restart - that the contract does not specify at all.

``test_restart_durability.py`` belongs to this category too but deliberately
keeps both its own file and its own name. It boots a private server so it can
stop and restart the binary mid-test, and that damages the shared session server
for every test collected after it - so it is only safe while its filename sorts
last in this directory. Renaming it into a ``test_operational_*`` form moved it
earlier in collection and broke 39 downstream tests. Leave the name alone until
that test is made genuinely isolated; new operational tests go here instead.

Conventions for anything added here:

- Put the bound in a module constant and say, in the constant's comment, where
  the figure came from and what it does NOT prove. A number not derived from a
  scenario or from DESIGN.md is a working figure, not a guarantee, and has to be
  labelled as one so a later reader does not mistake it for contract.
- Keep each test's subject narrow enough that a failure names one property.
- If a scenario is later written for one of these properties, move that test out
  of this file and tag it with the scenario, so this file stays exactly the set
  of things the contract is silent about.
"""

from __future__ import annotations

import time
import uuid
from datetime import datetime, timezone

import httpx
import pytest

from .conftest import EVENT_TYPE_LONGPOLL, SUBJECT_TYPE, TOPIC_LONGPOLL, SseFrameReader

# Ceiling on publish-ack -> delivery for an event published onto an already-open
# stream. A working figure, not a documented guarantee: no scenario states a
# delivery-latency bound, and DESIGN.md fixes only the 30s long-poll max timeout.
#
# It sits ABOVE the 5s heartbeat cadence, so it proves delivery happens at all
# and nothing more - it cannot tell an active wake-up apart from delivery that
# rode the next heartbeat. Proving the wake-up itself needs a sub-5s bound, and
# that number has to come from a scenario before a test can assert it.
DELIVERY_BOUND = 15.0


@pytest.mark.timeout(60, func_only=True)
async def test_published_event_reaches_open_stream_within_bound(api, test_env):
    """An event published while a stream is open is delivered within the bound.

    The subscription covers only TOPIC_LONGPOLL and no other test publishes
    there, so the single event this test publishes is the only one it can
    receive - the measurement is not polluted by another test's traffic.
    """
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
                        "topic": TOPIC_LONGPOLL,
                        "tenant_id": tenant_id,
                        "types": [EVENT_TYPE_LONGPOLL],
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
                    {"topic": TOPIC_LONGPOLL, "partition": 0, "value": "earliest"}
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
            topology_kind, _ = await reader.next_frame(timeout=5)
            assert topology_kind == "topology"

            event_id = str(uuid.uuid4())
            occurred_at = datetime.now(timezone.utc).isoformat()
            async with api() as publish_client:
                publish_resp = await publish_client.post(
                    "/events",
                    json={
                        "id": event_id,
                        "type": EVENT_TYPE_LONGPOLL,
                        "tenant_id": tenant_id,
                        "source": "e2e-test",
                        "subject": "s1",
                        "subject_type": SUBJECT_TYPE,
                        "occurred_at": occurred_at,
                    },
                )
            assert publish_resp.status_code == 202
            # Timed from the ack, so the bound covers ingest and sequencing as
            # well as delivery - every step the publisher waits through.
            published_at = time.monotonic()

            event_data = await reader.await_kind("event", timeout=DELIVERY_BOUND)
            elapsed = time.monotonic() - published_at

    payload = event_data["payload"]
    assert payload == {
        "id": event_id,
        "type": EVENT_TYPE_LONGPOLL,
        "topic": TOPIC_LONGPOLL,
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
    assert elapsed < DELIVERY_BOUND, f"delivery took {elapsed:.2f}s"
