"""SEEK / cursor scenarios (consumer/positions/1.01-1.14).

All SEEK requests use the array wire format confirmed by existing tests:
``{"partition_positions": [{"topic": "...", "partition": int, "value": ...}]}``.
The scenarios show a dict format - that is scenario notation, not the wire format
the implementation accepts.

The SEEK response is also an array:
``[{"topic": "...", "partition": int, "value": int}]``
where ``value`` is the resolved cursor (RF-1 for "earliest", HWM for "latest").

Every test that asserts a concrete cursor value or a quoted valid range runs
against TOPIC_PREFILLED, which is seeded with PREFILLED_COUNT events into a
fresh database at session bootstrap. That fixes RF-1 at 0 and HWM at
PREFILLED_COUNT, so both bounds are known constants rather than whatever a
shared topic happens to hold.
"""

from __future__ import annotations

import uuid

import httpx
import pytest

from .conftest import (
    API_BASE,
    EVENT_TYPE_4P,
    EVENT_TYPE_STREAM,
    EVENT_TYPE_PREFILLED,
    TOPIC_4P,
    PREFILLED_COUNT,
    PREFILLED_TENANT_ID,
    SUBJECT_TYPE,
    TOPIC_STREAM,
    TOPIC_PREFILLED,
    SseFrameReader,
)


async def test_seek_earliest_returns_resolved_cursor(api, prefilled_topic):
    """scenario: consumer/positions/1.01-positive-seek-earliest.md

    "earliest" resolves to RF-1.  RF=1 throughout this test session (no
    retention eviction on TOPIC_PREFILLED), so the cursor is always 0 even
    though the topic carries PREFILLED_COUNT events.  Using a pre-seeded
    topic proves that "earliest" is 0 while "latest" (tested separately) is
    non-zero — the two sentinels resolve to distinct values.
    """
    async with api() as client:
        group_resp = await client.post("/consumer-groups")
        assert group_resp.status_code == 201
        group_id = group_resp.json()["id"]

        sub_resp = await client.post(
            "/subscriptions",
            json={
                "consumer_group": group_id,
                "client_agent": "e2e-test",
                "interests": [
                    {
                        "topic": TOPIC_PREFILLED,
                        "tenant_id": PREFILLED_TENANT_ID,
                        "types": [EVENT_TYPE_PREFILLED],
                    }
                ],
            },
        )
        assert sub_resp.status_code == 201
        sub_id = str(sub_resp.json()["id"])

        resp = await client.post(
            f"/subscriptions/{sub_id}:seek",
            json={
                "partition_positions": [
                    {"topic": TOPIC_PREFILLED, "partition": 0, "value": "earliest"}
                ]
            },
        )
    assert resp.status_code == 200
    body = resp.json()
    assert isinstance(body, list)
    assert len(body) == 1
    assert body[0] == {"topic": TOPIC_PREFILLED, "partition": 0, "value": 0}


async def test_seek_latest_returns_hwm_cursor(api, prefilled_topic):
    """scenario: consumer/positions/1.02-positive-seek-latest.md

    "latest" resolves to the current HWM.  TOPIC_PREFILLED is seeded with
    exactly PREFILLED_COUNT events at session bootstrap into a fresh database,
    so sequences start at 1 and the HWM is exactly PREFILLED_COUNT.
    """
    async with api() as client:
        group_resp = await client.post("/consumer-groups")
        assert group_resp.status_code == 201
        group_id = group_resp.json()["id"]

        sub_resp = await client.post(
            "/subscriptions",
            json={
                "consumer_group": group_id,
                "client_agent": "e2e-test",
                "interests": [
                    {
                        "topic": TOPIC_PREFILLED,
                        "tenant_id": PREFILLED_TENANT_ID,
                        "types": [EVENT_TYPE_PREFILLED],
                    }
                ],
            },
        )
        assert sub_resp.status_code == 201
        sub_id = str(sub_resp.json()["id"])

        resp = await client.post(
            f"/subscriptions/{sub_id}:seek",
            json={
                "partition_positions": [
                    {"topic": TOPIC_PREFILLED, "partition": 0, "value": "latest"}
                ]
            },
        )
    assert resp.status_code == 200
    body = resp.json()
    assert isinstance(body, list)
    assert len(body) == 1
    assert body[0] == {"topic": TOPIC_PREFILLED, "partition": 0, "value": PREFILLED_COUNT}


async def test_seek_exact_offset_accepted(api, prefilled_topic):
    """scenario: consumer/positions/1.03-positive-seek-exact-offset.md

    First seek to "earliest" to obtain a valid cursor integer, then re-seek
    to that same integer explicitly.  TOPIC_PREFILLED has RF=1 so "earliest"
    resolves to 0; the explicit re-seek also returns 0.
    """
    async with api() as client:
        group_resp = await client.post("/consumer-groups")
        assert group_resp.status_code == 201
        group_id = group_resp.json()["id"]

        sub_resp = await client.post(
            "/subscriptions",
            json={
                "consumer_group": group_id,
                "client_agent": "e2e-test",
                "interests": [
                    {
                        "topic": TOPIC_PREFILLED,
                        "tenant_id": PREFILLED_TENANT_ID,
                        "types": [EVENT_TYPE_PREFILLED],
                    }
                ],
            },
        )
        assert sub_resp.status_code == 201
        sub_id = str(sub_resp.json()["id"])

        earliest = await client.post(
            f"/subscriptions/{sub_id}:seek",
            json={
                "partition_positions": [
                    {"topic": TOPIC_PREFILLED, "partition": 0, "value": "earliest"}
                ]
            },
        )
        assert earliest.status_code == 200
        assert earliest.json()[0] == {"topic": TOPIC_PREFILLED, "partition": 0, "value": 0}

        # Re-seek to the same integer explicitly.
        resp = await client.post(
            f"/subscriptions/{sub_id}:seek",
            json={
                "partition_positions": [
                    {"topic": TOPIC_PREFILLED, "partition": 0, "value": 0}
                ]
            },
        )
    assert resp.status_code == 200
    assert resp.json()[0] == {"topic": TOPIC_PREFILLED, "partition": 0, "value": 0}


async def test_mixed_sentinels_and_integers_accepted(api, prefilled_topic):
    """scenario: consumer/positions/1.04-positive-mixed-sentinels.md

    A SEEK may send an integer alongside a sentinel in the same request.  With
    TOPIC_PREFILLED (1 partition) a true mix of two partitions is not possible,
    so the test demonstrates that an integer equivalent to a sentinel resolves
    to the same concrete value: seeking 0 (= "earliest") and seeking
    PREFILLED_COUNT (= "latest") both return the expected integers.
    """
    async with api() as client:
        group_resp = await client.post("/consumer-groups")
        assert group_resp.status_code == 201
        group_id = group_resp.json()["id"]

        sub_resp = await client.post(
            "/subscriptions",
            json={
                "consumer_group": group_id,
                "client_agent": "e2e-test",
                "interests": [
                    {
                        "topic": TOPIC_PREFILLED,
                        "tenant_id": PREFILLED_TENANT_ID,
                        "types": [EVENT_TYPE_PREFILLED],
                    }
                ],
            },
        )
        assert sub_resp.status_code == 201
        sub_id = str(sub_resp.json()["id"])

        # Integer equivalent of "earliest".
        resp_low = await client.post(
            f"/subscriptions/{sub_id}:seek",
            json={
                "partition_positions": [
                    {"topic": TOPIC_PREFILLED, "partition": 0, "value": 0}
                ]
            },
        )
        assert resp_low.status_code == 200
        assert resp_low.json()[0] == {"topic": TOPIC_PREFILLED, "partition": 0, "value": 0}

        # Integer equivalent of "latest".
        resp_high = await client.post(
            f"/subscriptions/{sub_id}:seek",
            json={
                "partition_positions": [
                    {"topic": TOPIC_PREFILLED, "partition": 0, "value": PREFILLED_COUNT}
                ]
            },
        )
    assert resp_high.status_code == 200
    assert resp_high.json()[0] == {"topic": TOPIC_PREFILLED, "partition": 0, "value": PREFILLED_COUNT}


async def test_out_of_range_below_rf_rejected_400(api, prefilled_topic):
    """scenario: consumer/positions/1.05-negative-out-of-range-offset.md

    An integer SEEK value below RF-1 is rejected.  TOPIC_PREFILLED fixes the
    valid range - RF-1 = 0 and HWM = PREFILLED_COUNT - so the range the error
    quotes is a known constant and can be asserted literally.  Seeking to -1
    is below the floor.  The error names the offending entry by its index in
    ``partition_positions`` and carries no part of the submitted value.
    """
    async with api() as client:
        group_resp = await client.post("/consumer-groups")
        assert group_resp.status_code == 201
        group_id = group_resp.json()["id"]

        sub_resp = await client.post(
            "/subscriptions",
            json={
                "consumer_group": group_id,
                "client_agent": "e2e-test",
                "interests": [
                    {
                        "topic": TOPIC_PREFILLED,
                        "tenant_id": PREFILLED_TENANT_ID,
                        "types": [EVENT_TYPE_PREFILLED],
                    }
                ],
            },
        )
        assert sub_resp.status_code == 201
        sub_id = str(sub_resp.json()["id"])

        resp = await client.post(
            f"/subscriptions/{sub_id}:seek",
            json={
                "partition_positions": [
                    {"topic": TOPIC_PREFILLED, "partition": 0, "value": -1}
                ]
            },
        )
    assert resp.status_code == 400
    body = resp.json()
    assert body == {
        "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~",
        "title": "Invalid Argument",
        "status": 400,
        "detail": "Request validation failed",
        "instance": f"{API_BASE}/subscriptions/{sub_id}:seek",
        "trace_id": body["trace_id"],
        "context": {
            "field_violations": [
                {
                    "field": "partition_positions[0].value",
                    "description": f"the position is below the valid range [0, {PREFILLED_COUNT}]",
                    "reason": "below_retention_floor",
                }
            ],
            "resource_type": "gts.cf.core.events.request.v1~",
        },
    }


async def test_offset_above_hwm_rejected_400(api, prefilled_topic):
    """scenario: consumer/positions/1.06-negative-offset-above-hwm.md

    A SEEK value above HWM is rejected - it would claim to have processed
    events that do not exist yet.  TOPIC_PREFILLED puts HWM at exactly
    PREFILLED_COUNT, so seeking one past it is the tightest possible breach
    and the quoted range is a known constant.
    """
    async with api() as client:
        group_resp = await client.post("/consumer-groups")
        assert group_resp.status_code == 201
        group_id = group_resp.json()["id"]

        sub_resp = await client.post(
            "/subscriptions",
            json={
                "consumer_group": group_id,
                "client_agent": "e2e-test",
                "interests": [
                    {
                        "topic": TOPIC_PREFILLED,
                        "tenant_id": PREFILLED_TENANT_ID,
                        "types": [EVENT_TYPE_PREFILLED],
                    }
                ],
            },
        )
        assert sub_resp.status_code == 201
        sub_id = str(sub_resp.json()["id"])

        resp = await client.post(
            f"/subscriptions/{sub_id}:seek",
            json={
                "partition_positions": [
                    {"topic": TOPIC_PREFILLED, "partition": 0, "value": PREFILLED_COUNT + 1}
                ]
            },
        )
    assert resp.status_code == 400
    body = resp.json()
    assert body == {
        "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~",
        "title": "Invalid Argument",
        "status": 400,
        "detail": "Request validation failed",
        "instance": f"{API_BASE}/subscriptions/{sub_id}:seek",
        "trace_id": body["trace_id"],
        "context": {
            "field_violations": [
                {
                    "field": "partition_positions[0].value",
                    "description": f"the position is above the valid range [0, {PREFILLED_COUNT}]",
                    "reason": "above_high_water_mark",
                }
            ],
            "resource_type": "gts.cf.core.events.request.v1~",
        },
    }


@pytest.mark.timeout(15, func_only=True)
async def test_seek_while_streaming_returns_409(api, test_env):
    """scenario: consumer/positions/1.07-negative-seek-while-streaming.md

    SEEK is rejected while a stream is open on the same subscription.
    Opens an SSE stream in the background, sends a SEEK, asserts 409
    StreamingInProgress, then closes the stream.
    """
    async with api() as client:
        group_resp = await client.post("/consumer-groups")
        assert group_resp.status_code == 201
        group_id = group_resp.json()["id"]

        sub_resp = await client.post(
            "/subscriptions",
            json={
                "consumer_group": group_id,
                "client_agent": "e2e-test",
                "interests": [
                    {
                        "topic": TOPIC_STREAM,
                        "tenant_id": str(uuid.uuid4()),
                        "types": [EVENT_TYPE_STREAM],
                    }
                ],
            },
        )
        assert sub_resp.status_code == 201
        sub_id = str(sub_resp.json()["id"])

        # Seed the cursor so the stream can open.
        await client.post(
            f"/subscriptions/{sub_id}:seek",
            json={
                "partition_positions": [
                    {"topic": TOPIC_STREAM, "partition": 0, "value": "earliest"}
                ]
            },
        )

    # Open an SSE stream and hold it open while we attempt a SEEK.
    async with httpx.AsyncClient(
        base_url=f"{test_env.base_url}/event-broker/v1", timeout=None
    ) as stream_client:
        async with stream_client.stream(
            "GET", f"/events:sse?subscription_id={sub_id}"
        ) as stream_resp:
            assert stream_resp.status_code == 200

            async with api() as seek_client:
                seek_resp = await seek_client.post(
                    f"/subscriptions/{sub_id}:seek",
                    json={
                        "partition_positions": [
                            {"topic": TOPIC_STREAM, "partition": 0, "value": "earliest"}
                        ]
                    },
                )

    assert seek_resp.status_code == 409
    body = seek_resp.json()
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


async def test_seek_unknown_subscription_returns_404(api):
    """scenario: consumer/positions/1.08-negative-seek-unknown-subscription.md

    SEEK on a subscription that does not exist returns 404 Not Found.
    """
    fake_sub_id = str(uuid.uuid4())
    async with api() as client:
        resp = await client.post(
            f"/subscriptions/{fake_sub_id}:seek",
            json={
                "partition_positions": [
                    {"topic": TOPIC_STREAM, "partition": 0, "value": "earliest"}
                ]
            },
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


async def test_seek_unassigned_partition_rejected_409(api):
    """scenario: consumer/positions/1.09-negative-seek-unassigned-partition.md

    The partition must exist and belong to *another member* - that is what
    "unassigned" means here.  Two members split TOPIC_4P's four partitions, so
    each holds a real partition the other does not, and the first member SEEKs
    one of the second's.

    Seeking a partition index the topic does not have at all is a different
    condition and would not exercise this rejection.
    """
    tenant_id = str(uuid.uuid4())
    interest = {
        "topic": TOPIC_4P,
        "tenant_id": tenant_id,
        "types": [EVENT_TYPE_4P],
    }
    async with api() as client:
        group_resp = await client.post("/consumer-groups")
        assert group_resp.status_code == 201
        group_id = group_resp.json()["id"]

        sub1_resp = await client.post(
            "/subscriptions",
            json={"consumer_group": group_id, "client_agent": "e2e-test-1", "interests": [interest]},
        )
        assert sub1_resp.status_code == 201
        sub1_id = str(sub1_resp.json()["id"])

        sub2_resp = await client.post(
            "/subscriptions",
            json={"consumer_group": group_id, "client_agent": "e2e-test-2", "interests": [interest]},
        )
        assert sub2_resp.status_code == 201
        sub2_partitions = [a["partition"] for a in sub2_resp.json()["assigned"]]
        assert sub2_partitions, "second member must hold at least one partition to borrow"

        resp = await client.post(
            f"/subscriptions/{sub1_id}:seek",
            json={
                "partition_positions": [
                    {"topic": TOPIC_4P, "partition": sub2_partitions[0], "value": "earliest"}
                ]
            },
        )
    assert resp.status_code == 409
    body = resp.json()
    assert body == {
        "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.failed_precondition.v1~",
        "title": "Failed Precondition",
        "status": 409,
        "detail": body["detail"],
        "instance": body["instance"],
        "trace_id": body["trace_id"],
        "context": body["context"],
    }


async def test_seek_any_value_in_retention_range_accepted(api, prefilled_topic):
    """scenario: consumer/positions/1.10-positive-seek-any-value-in-range.md

    Any integer in [RF-1, HWM] is accepted.  TOPIC_PREFILLED has RF-1=0 and
    HWM=PREFILLED_COUNT so both boundaries are known; a midpoint (PREFILLED_COUNT//2)
    confirms an interior value is also accepted.
    """
    async with api() as client:
        group_resp = await client.post("/consumer-groups")
        assert group_resp.status_code == 201
        group_id = group_resp.json()["id"]

        sub_resp = await client.post(
            "/subscriptions",
            json={
                "consumer_group": group_id,
                "client_agent": "e2e-test",
                "interests": [
                    {
                        "topic": TOPIC_PREFILLED,
                        "tenant_id": PREFILLED_TENANT_ID,
                        "types": [EVENT_TYPE_PREFILLED],
                    }
                ],
            },
        )
        assert sub_resp.status_code == 201
        sub_id = str(sub_resp.json()["id"])

        for value in (0, PREFILLED_COUNT // 2, PREFILLED_COUNT):
            resp = await client.post(
                f"/subscriptions/{sub_id}:seek",
                json={
                    "partition_positions": [
                        {"topic": TOPIC_PREFILLED, "partition": 0, "value": value}
                    ]
                },
            )
            assert resp.status_code == 200
            assert resp.json()[0] == {"topic": TOPIC_PREFILLED, "partition": 0, "value": value}


async def test_seek_at_timestamp_returns_resolved_integer(api, prefilled_topic):
    """scenario: consumer/positions/1.11-positive-seek-at-timestamp.md

    "at:<ISO-8601>" resolves to the first event at or after that timestamp.
    A far-future timestamp clamps to HWM.  TOPIC_PREFILLED is seeded with
    PREFILLED_COUNT events so the HWM is exactly PREFILLED_COUNT and the
    assertion is concrete.
    """
    async with api() as client:
        group_resp = await client.post("/consumer-groups")
        assert group_resp.status_code == 201
        group_id = group_resp.json()["id"]

        sub_resp = await client.post(
            "/subscriptions",
            json={
                "consumer_group": group_id,
                "client_agent": "e2e-test",
                "interests": [
                    {
                        "topic": TOPIC_PREFILLED,
                        "tenant_id": PREFILLED_TENANT_ID,
                        "types": [EVENT_TYPE_PREFILLED],
                    }
                ],
            },
        )
        assert sub_resp.status_code == 201
        sub_id = str(sub_resp.json()["id"])

        resp = await client.post(
            f"/subscriptions/{sub_id}:seek",
            json={
                "partition_positions": [
                    {"topic": TOPIC_PREFILLED, "partition": 0, "value": "at:2099-01-01T00:00:00Z"}
                ]
            },
        )
    assert resp.status_code == 200
    body = resp.json()
    assert isinstance(body, list)
    assert len(body) == 1
    assert body[0] == {"topic": TOPIC_PREFILLED, "partition": 0, "value": PREFILLED_COUNT}


async def test_seek_timestamp_before_retention_clamps_to_rf(api, prefilled_topic):
    """scenario: consumer/positions/1.12-positive-seek-at-timestamp-before-retention.md

    A timestamp before the retention floor clamps to RF-1.  TOPIC_PREFILLED
    has PREFILLED_COUNT events but RF=1 (no eviction in this session), so
    RF-1=0 and a pre-epoch timestamp resolves to exactly 0.
    """
    async with api() as client:
        group_resp = await client.post("/consumer-groups")
        assert group_resp.status_code == 201
        group_id = group_resp.json()["id"]

        sub_resp = await client.post(
            "/subscriptions",
            json={
                "consumer_group": group_id,
                "client_agent": "e2e-test",
                "interests": [
                    {
                        "topic": TOPIC_PREFILLED,
                        "tenant_id": PREFILLED_TENANT_ID,
                        "types": [EVENT_TYPE_PREFILLED],
                    }
                ],
            },
        )
        assert sub_resp.status_code == 201
        sub_id = str(sub_resp.json()["id"])

        resp = await client.post(
            f"/subscriptions/{sub_id}:seek",
            json={
                "partition_positions": [
                    {"topic": TOPIC_PREFILLED, "partition": 0, "value": "at:1970-01-01T00:00:00Z"}
                ]
            },
        )
    assert resp.status_code == 200
    body = resp.json()
    assert isinstance(body, list)
    assert len(body) == 1
    assert body[0] == {"topic": TOPIC_PREFILLED, "partition": 0, "value": 0}


async def test_seek_timestamp_beyond_hwm_clamps_to_hwm(api, prefilled_topic):
    """scenario: consumer/positions/1.13-positive-seek-at-timestamp-beyond-hwm.md

    A timestamp beyond the HWM clamps to the current HWM (same as "latest").
    TOPIC_PREFILLED's HWM is exactly PREFILLED_COUNT so the assertion is
    concrete; no need to first resolve "latest" and compare indirectly.
    """
    async with api() as client:
        group_resp = await client.post("/consumer-groups")
        assert group_resp.status_code == 201
        group_id = group_resp.json()["id"]

        sub_resp = await client.post(
            "/subscriptions",
            json={
                "consumer_group": group_id,
                "client_agent": "e2e-test",
                "interests": [
                    {
                        "topic": TOPIC_PREFILLED,
                        "tenant_id": PREFILLED_TENANT_ID,
                        "types": [EVENT_TYPE_PREFILLED],
                    }
                ],
            },
        )
        assert sub_resp.status_code == 201
        sub_id = str(sub_resp.json()["id"])

        resp = await client.post(
            f"/subscriptions/{sub_id}:seek",
            json={
                "partition_positions": [
                    {"topic": TOPIC_PREFILLED, "partition": 0, "value": "at:2099-12-31T23:59:59Z"}
                ]
            },
        )
    assert resp.status_code == 200
    body = resp.json()
    assert isinstance(body, list)
    assert len(body) == 1
    assert body[0] == {"topic": TOPIC_PREFILLED, "partition": 0, "value": PREFILLED_COUNT}


RESUME_AT = 40


@pytest.mark.timeout(30, func_only=True)
async def test_seek_resume_from_cursor(api, test_env, prefilled_topic):
    """scenario: consumer/positions/1.14-positive-seek-resume-from-cursor.md

    Consumer reads from earliest to sequence RESUME_AT, then reconnects in the
    same group by seeking to the integer RESUME_AT. The first event on the
    resumed stream must be sequence RESUME_AT + 1 - no event reprocessed, none
    skipped.
    """
    # ── Phase 1: join, seek earliest, stream until sequence RESUME_AT ──────
    async with api() as client:
        group_resp = await client.post("/consumer-groups")
        assert group_resp.status_code == 201
        group_id = group_resp.json()["id"]

        sub1_resp = await client.post(
            "/subscriptions",
            json={
                "consumer_group": group_id,
                "client_agent": "e2e-test",
                "interests": [
                    {
                        "topic": TOPIC_PREFILLED,
                        "tenant_id": PREFILLED_TENANT_ID,
                        "types": [EVENT_TYPE_PREFILLED],
                    }
                ],
            },
        )
        assert sub1_resp.status_code == 201
        sub1_id = str(sub1_resp.json()["id"])

        seek1_resp = await client.post(
            f"/subscriptions/{sub1_id}:seek",
            json={
                "partition_positions": [
                    {"topic": TOPIC_PREFILLED, "partition": 0, "value": "earliest"}
                ]
            },
        )
        assert seek1_resp.status_code == 200
        assert seek1_resp.json() == [
            {"topic": TOPIC_PREFILLED, "partition": 0, "value": 0}
        ]

    async with httpx.AsyncClient(
        base_url=f"{test_env.base_url}/event-broker/v1", timeout=None
    ) as stream_client:
        async with stream_client.stream(
            "GET", f"/events:sse?subscription_id={sub1_id}"
        ) as stream_resp:
            assert stream_resp.status_code == 200
            reader = SseFrameReader(stream_resp)
            topology_kind, _ = await reader.next_frame(timeout=5)
            assert topology_kind == "topology"
            while True:
                kind, data = await reader.next_frame(timeout=10)
                if kind == "event" and data["payload"]["sequence"] == RESUME_AT:
                    break

    # ── Phase 2: delete sub1, create sub2, seek to RESUME_AT, resume ───────
    async with api() as client:
        del_resp = await client.delete(f"/subscriptions/{sub1_id}")
        assert del_resp.status_code == 204

        sub2_resp = await client.post(
            "/subscriptions",
            json={
                "consumer_group": group_id,
                "client_agent": "e2e-test",
                "interests": [
                    {
                        "topic": TOPIC_PREFILLED,
                        "tenant_id": PREFILLED_TENANT_ID,
                        "types": [EVENT_TYPE_PREFILLED],
                    }
                ],
            },
        )
        assert sub2_resp.status_code == 201
        sub2_id = str(sub2_resp.json()["id"])

        seek2_resp = await client.post(
            f"/subscriptions/{sub2_id}:seek",
            json={
                "partition_positions": [
                    {"topic": TOPIC_PREFILLED, "partition": 0, "value": RESUME_AT}
                ]
            },
        )
        assert seek2_resp.status_code == 200
        assert seek2_resp.json() == [
            {"topic": TOPIC_PREFILLED, "partition": 0, "value": RESUME_AT}
        ]

    async with httpx.AsyncClient(
        base_url=f"{test_env.base_url}/event-broker/v1", timeout=None
    ) as stream_client:
        async with stream_client.stream(
            "GET", f"/events:sse?subscription_id={sub2_id}"
        ) as stream_resp:
            assert stream_resp.status_code == 200
            reader = SseFrameReader(stream_resp)
            topology_kind, _ = await reader.next_frame(timeout=5)
            assert topology_kind == "topology"
            event_kind, event_data = await reader.next_frame(timeout=5)

    assert event_kind == "event"
    payload = event_data["payload"]
    assert payload == {
        "id": payload["id"],
        "type": EVENT_TYPE_PREFILLED,
        "topic": TOPIC_PREFILLED,
        "tenant_id": PREFILLED_TENANT_ID,
        "source": "e2e-prefill",
        "subject": "prefill-subject",
        "subject_type": SUBJECT_TYPE,
        "occurred_at": payload["occurred_at"],
        "trace_parent": None,
        "data": None,
        "partition": 0,
        "sequence": RESUME_AT + 1,
        "sequence_time": payload["sequence_time"],
    }
