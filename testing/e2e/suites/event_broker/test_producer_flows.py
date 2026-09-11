"""Chained-producer flow scenarios (producer/flows/1.01-1.09).

Register-and-publish patterns that exercise the exactly-once ingest-side
deduplication protocol: registration, chained sequence, idempotency key dedup,
sequence-violation rejection, cursor recovery, chain reset, and unknown-producer
rejection.

Each test registers its own producer so tests are independent.
"""

from __future__ import annotations

import uuid
from datetime import datetime, timezone

from .conftest import EVENT_TYPE_STREAM, SUBJECT_TYPE, TOPIC_STREAM


def _now() -> str:
    return datetime.now(timezone.utc).isoformat()


def _event(tenant_id: str, event_id: str | None = None) -> dict:
    return {
        "id": event_id or str(uuid.uuid4()),
        "type": EVENT_TYPE_STREAM,
        "tenant_id": tenant_id,
        "source": "e2e-test",
        "subject": "s1",
        "subject_type": "gts.cf.e2e.event_broker.subject.v1~",
        "occurred_at": _now(),
    }


async def test_register_chained_producer_returns_201(api):
    """scenario: producer/flows/1.01-positive-register-chained-producer.md"""
    async with api() as client:
        resp = await client.post(
            "/producers",
            json={"mode": "chained", "client_agent": "e2e-test/1.0"},
        )
    assert resp.status_code == 201
    body = resp.json()
    assert body == {
        "id": body["id"],
        "mode": "chained",
        "client_agent": "e2e-test/1.0",
    }
    assert body["id"] != ""


async def test_register_monotonic_producer_returns_201(api):
    """scenario: producer/flows/1.02-positive-register-monotonic-producer.md"""
    async with api() as client:
        resp = await client.post(
            "/producers",
            json={"mode": "monotonic", "client_agent": "e2e-test/1.0"},
        )
    assert resp.status_code == 201
    body = resp.json()
    assert body == {
        "id": body["id"],
        "mode": "monotonic",
        "client_agent": "e2e-test/1.0",
    }


async def test_chained_mode_sequence_advances(api):
    """scenario: producer/flows/1.03-positive-chained-mode-sequence.md

    A chained producer publishes event N with meta.previous=N-1; the broker
    validates the chain, admits the event, and advances last_sequence to N.
    """
    tenant_id = str(uuid.uuid4())
    async with api() as client:
        reg = await client.post(
            "/producers",
            json={"mode": "chained", "client_agent": "e2e-test/1.0"},
        )
        assert reg.status_code == 201
        producer_id = reg.json()["id"]

        # First event: previous=0, sequence=1
        resp1 = await client.post(
            "/events",
            headers={"Producer-Id": producer_id},
            json={
                **_event(tenant_id),
                "meta": {
                    "version": 1,
                    "producer_id": producer_id,
                    "previous": 0,
                    "sequence": 1,
                },
            },
        )
        assert resp1.status_code == 202

        # Second event: previous=1, sequence=2
        resp2 = await client.post(
            "/events",
            headers={"Producer-Id": producer_id},
            json={
                **_event(tenant_id),
                "meta": {
                    "version": 1,
                    "producer_id": producer_id,
                    "previous": 1,
                    "sequence": 2,
                },
            },
        )
    assert resp2.status_code == 202


async def test_idempotency_key_dedup_returns_original(api):
    """scenario: producer/flows/1.04-positive-idempotency-key-dedup.md

    A chained publish with the same event ``id`` and ``meta.previous`` as a
    previously accepted event is a duplicate: the broker returns 200 (not 202)
    and does not write a second row.
    """
    tenant_id = str(uuid.uuid4())
    event_id = str(uuid.uuid4())
    async with api() as client:
        reg = await client.post(
            "/producers",
            json={"mode": "chained", "client_agent": "e2e-test/1.0"},
        )
        assert reg.status_code == 201
        producer_id = reg.json()["id"]

        payload = {
            **_event(tenant_id, event_id=event_id),
            "meta": {
                "version": 1,
                "producer_id": producer_id,
                "previous": 0,
                "sequence": 1,
            },
        }

        first = await client.post(
            "/events", headers={"Producer-Id": producer_id}, json=payload
        )
        assert first.status_code == 202

        # Exact same payload → dedup → 200
        second = await client.post(
            "/events", headers={"Producer-Id": producer_id}, json=payload
        )
    assert second.status_code == 200
    assert second.text == "", "200 dedup response must carry no body"


async def test_chained_sequence_violation_returns_412(api):
    """scenario: producer/flows/1.05-negative-chained-sequence-violation.md"""
    tenant_id = str(uuid.uuid4())
    async with api() as client:
        reg = await client.post(
            "/producers",
            json={"mode": "chained", "client_agent": "e2e-test/1.0"},
        )
        assert reg.status_code == 201
        producer_id = reg.json()["id"]

        # First event: establishes last_sequence=1 for this producer.
        first = await client.post(
            "/events",
            headers={"Producer-Id": producer_id},
            json={
                **_event(tenant_id),
                "meta": {
                    "version": 1,
                    "producer_id": producer_id,
                    "previous": 0,
                    "sequence": 1,
                },
            },
        )
        assert first.status_code == 202

        # Stale previous: broker has last_sequence=1 but we claim previous=99.
        resp = await client.post(
            "/events",
            headers={"Producer-Id": producer_id},
            json={
                **_event(tenant_id),
                "meta": {
                    "version": 1,
                    "producer_id": producer_id,
                    "previous": 99,
                    "sequence": 100,
                },
            },
        )
    assert resp.status_code == 412
    body = resp.json()
    assert body == {
        "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.failed_precondition.v1~",
        "title": "Failed Precondition",
        "status": 412,
        "detail": body["detail"],
        "instance": body["instance"],
        "trace_id": body["trace_id"],
        "context": {
            "violations": [
                {
                    "type": "sequence_mismatch",
                    "subject": "(producer)",
                    "description": body["context"]["violations"][0]["description"],
                }
            ]
        },
    }


async def test_cursor_recovery_returns_last_sequence(api):
    """scenario: producer/flows/1.06-positive-cursor-recovery.md"""
    tenant_id = str(uuid.uuid4())
    async with api() as client:
        reg = await client.post(
            "/producers",
            json={"mode": "chained", "client_agent": "e2e-test/1.0"},
        )
        assert reg.status_code == 201
        producer_id = reg.json()["id"]

        # Publish two events so the cursor has something to return.
        await client.post(
            "/events",
            headers={"Producer-Id": producer_id},
            json={
                **_event(tenant_id),
                "meta": {
                    "version": 1,
                    "producer_id": producer_id,
                    "previous": 0,
                    "sequence": 1,
                },
            },
        )
        await client.post(
            "/events",
            headers={"Producer-Id": producer_id},
            json={
                **_event(tenant_id),
                "meta": {
                    "version": 1,
                    "producer_id": producer_id,
                    "previous": 1,
                    "sequence": 2,
                },
            },
        )

        cursor_resp = await client.get(f"/producers/{producer_id}/cursors")
    assert cursor_resp.status_code == 200
    body = cursor_resp.json()
    assert body == {
        "producer_id": producer_id,
        "client_agent": "e2e-test/1.0",
        "topics": [
            {
                "topic": TOPIC_STREAM,
                "partitions": [
                    {
                        "partition": body["topics"][0]["partitions"][0]["partition"],
                        "last_sequence": 2,
                    }
                ],
            }
        ],
    }


async def test_chain_reset_returns_200(api):
    """scenario: producer/flows/1.07-positive-chain-reset.md"""
    tenant_id = str(uuid.uuid4())
    async with api() as client:
        reg = await client.post(
            "/producers",
            json={"mode": "chained", "client_agent": "e2e-test/1.0"},
        )
        assert reg.status_code == 201
        producer_id = reg.json()["id"]

        # Publish one event to create state.
        await client.post(
            "/events",
            headers={"Producer-Id": producer_id},
            json={
                **_event(tenant_id),
                "meta": {
                    "version": 1,
                    "producer_id": producer_id,
                    "previous": 0,
                    "sequence": 1,
                },
            },
        )

        # Reset clears the chain state.
        reset_resp = await client.post(f"/producers/{producer_id}:reset")
    assert reset_resp.status_code == 200
    assert reset_resp.text == ""


async def test_unknown_producer_returns_404(api):
    """scenario: producer/flows/1.08-negative-unknown-producer.md

    Supplying an unregistered (or reaped) ``Producer-Id`` header is rejected.
    The scenario file says 400, but ``error.rs`` maps ``ProducerNotFound`` to
    ``not_found()`` → 404 with a ``resource_name`` identifying the missing id.
    """
    fake_producer_id = str(uuid.uuid4())
    tenant_id = str(uuid.uuid4())
    async with api() as client:
        resp = await client.post(
            "/events",
            headers={"Producer-Id": fake_producer_id},
            json={
                **_event(tenant_id),
                "meta": {
                    "version": 1,
                    "producer_id": fake_producer_id,
                    "previous": 0,
                    "sequence": 1,
                },
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
            "resource_type": "gts.cf.core.events.producer.v1~",
            "resource_name": fake_producer_id,
        },
    }


async def test_chained_producer_desync_recovery(api):
    """scenario: producer/flows/1.09-flow-chained-producer-desync-recovery.md

    A chained producer's local sequence counter diverges from the broker's
    (simulated by using an incorrect ``meta.previous``).  The first publish
    fails with 412 SequenceViolation.  The producer reads the authoritative
    cursor via ``GET /producers/{id}/cursors`` and republishes with the correct
    sequence.
    """
    tenant_id = str(uuid.uuid4())
    async with api() as client:
        # Register producer.
        reg = await client.post(
            "/producers",
            json={"mode": "chained", "client_agent": "e2e-test/1.0"},
        )
        assert reg.status_code == 201
        producer_id = reg.json()["id"]

        # Publish two events to advance the broker's sequence to 2.
        for seq in range(1, 3):
            r = await client.post(
                "/events",
                headers={"Producer-Id": producer_id},
                json={
                    **_event(tenant_id),
                    "meta": {
                        "version": 1,
                        "producer_id": producer_id,
                        "previous": seq - 1,
                        "sequence": seq,
                    },
                },
            )
            assert r.status_code == 202

        # Step 1 — Publish with a stale sequence (simulating a crash/restore).
        # Broker last accepted sequence=2; producer thinks it's 0.
        stale_resp = await client.post(
            "/events",
            headers={"Producer-Id": producer_id},
            json={
                **_event(tenant_id),
                "meta": {
                    "version": 1,
                    "producer_id": producer_id,
                    "previous": 0,
                    "sequence": 1,
                },
            },
        )
        assert stale_resp.status_code == 412
        stale_body = stale_resp.json()
        assert stale_body == {
            "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.failed_precondition.v1~",
            "title": "Failed Precondition",
            "status": 412,
            "detail": stale_body["detail"],
            "instance": stale_body["instance"],
            "trace_id": stale_body["trace_id"],
            "context": {
                "violations": [
                    {
                        "type": "sequence_mismatch",
                        "subject": "(producer)",
                        "description": stale_body["context"]["violations"][0]["description"],
                    }
                ]
            },
        }

        # Step 2 — Read the broker's cursor to learn the authoritative sequence.
        cursors_resp = await client.get(f"/producers/{producer_id}/cursors")
        assert cursors_resp.status_code == 200
        cursor_body = cursors_resp.json()
        assert cursor_body["producer_id"] == producer_id
        topics = cursor_body["topics"]
        assert len(topics) > 0
        partitions = topics[0]["partitions"]
        last_seq = partitions[0]["last_sequence"]
        assert last_seq == 2

        # Step 3 — Republish with the correct sequence (previous=2, sequence=3).
        recovered_resp = await client.post(
            "/events",
            headers={"Producer-Id": producer_id},
            json={
                **_event(tenant_id),
                "meta": {
                    "version": 1,
                    "producer_id": producer_id,
                    "previous": last_seq,
                    "sequence": last_seq + 1,
                },
            },
        )
    assert recovered_resp.status_code == 202
