"""Batch publish scenarios (producer/batch/1.01-1.04).

- 1.01: batch of two events, same topic → 202
- 1.02: batch mixing topics → 400 (the two pre-provisioned topics serve as
        the two different topics needed to trigger the rejection)
- 1.03: batch too large (> 100 events) → 413
- 1.04: late validation failure (invalid event at position N) → 400
"""

from __future__ import annotations

import uuid
from datetime import datetime, timezone

from .conftest import (
    API_BASE,
    EVENT_TYPE_STREAM,
    EVENT_TYPE_LONGPOLL,
    EVENT_TYPE_STRICT,
    SUBJECT_TYPE,
    TOPIC_STREAM,
    TOPIC_LONGPOLL,
)


def _event(event_type: str, tenant_id: str, subject: str = "s1") -> dict:
    return {
        "id": str(uuid.uuid4()),
        "type": event_type,
        "tenant_id": tenant_id,
        "source": "e2e-test",
        "subject": subject,
        "subject_type": SUBJECT_TYPE,
        "occurred_at": datetime.now(timezone.utc).isoformat(),
    }


async def test_batch_publish_returns_202(api):
    """scenario: producer/batch/1.01-positive-publish-batch.md"""
    tenant_id = str(uuid.uuid4())
    async with api() as client:
        resp = await client.post(
            "/events:batch",
            json={
                "events": [
                    _event(EVENT_TYPE_STREAM, tenant_id, "s1"),
                    _event(EVENT_TYPE_STREAM, tenant_id, "s2"),
                ]
            },
        )
    assert resp.status_code == 202
    assert resp.text == "", "202 Accepted must carry no body"


async def test_batch_mixing_topics_rejected_400(api):
    """scenario: producer/batch/1.02-negative-mixed-partition-batch.md

    TOPIC_STREAM and TOPIC_LONGPOLL are different topics; mixing event types
    from different topics in one batch is rejected 400.
    """
    tenant_id = str(uuid.uuid4())
    async with api() as client:
        resp = await client.post(
            "/events:batch",
            json={
                "events": [
                    _event(EVENT_TYPE_STREAM, tenant_id),
                    _event(EVENT_TYPE_LONGPOLL, tenant_id),
                ]
            },
        )
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


async def test_batch_too_large_rejected_413(api):
    """scenario: producer/batch/1.03-negative-batch-too-large.md

    101 events in one batch exceeds the 100-event limit; the broker returns 413.
    """
    tenant_id = str(uuid.uuid4())
    async with api() as client:
        resp = await client.post(
            "/events:batch",
            json={"events": [_event(EVENT_TYPE_STREAM, tenant_id) for _ in range(101)]},
        )
    assert resp.status_code == 413
    body = resp.json()
    assert body == {
        "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~",
        "title": "Invalid Argument",
        "status": 413,
        "detail": body["detail"],
        "instance": body["instance"],
        "trace_id": body["trace_id"],
        "context": body["context"],
    }


async def test_batch_late_validation_failure_rejected_422(api):
    """scenario: producer/batch/1.04-negative-batch-late-validation-failure.md

    A batch is all-or-nothing: the first event is valid on its own, and the
    second one's payload fails its type's schema, so neither is admitted.

    The failure has to be a *payload* violation for this to test what the
    scenario describes.  EVENT_TYPE_STRICT requires ``data.strict_field``, so
    omitting it is exactly that, and it resolves to ``422`` - envelope and
    read-only violations are the ``400`` family and are covered separately by
    the single-publish read-only test.
    """
    tenant_id = str(uuid.uuid4())
    valid = {**_event(EVENT_TYPE_STRICT, tenant_id), "data": {"strict_field": "ok"}}
    invalid = {**_event(EVENT_TYPE_STRICT, tenant_id), "data": {}}
    async with api() as client:
        resp = await client.post(
            "/events:batch",
            json={"events": [valid, invalid]},
        )
    assert resp.status_code == 422
    body = resp.json()
    assert body == {
        "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~",
        "title": "Invalid Argument",
        "status": 422,
        "detail": body["detail"],
        "instance": f"{API_BASE}/events:batch",
        "trace_id": body["trace_id"],
        "context": {
            "field_violations": [
                {
                    "field": "(payload)",
                    "description": body["context"]["field_violations"][0]["description"],
                    "reason": "schema_validation",
                }
            ]
        },
    }
