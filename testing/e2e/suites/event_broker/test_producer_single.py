"""Single-event publish scenarios (producer/single/1.01-1.05).

Each test exercises one publish path:
- 1.01: async publish → 202, no body
- 1.02: sync publish (wait=persisted) → 201
- 1.03: schema validation failure → 400 (requires EVENT_TYPE_STRICT)
- 1.04: rate limiting → 429 (xfail: rate limiting not configured in standalone)
- 1.05: read-only ``partition`` field rejected → 400
"""

from __future__ import annotations

import uuid
from datetime import datetime, timezone

import pytest

from .conftest import (
    SKIP_LIMITS,
    EVENT_TYPE_STREAM,
    EVENT_TYPE_STRICT,
    SUBJECT_TYPE,
    TOPIC_STREAM,
    TOPIC_STRICT,
)


def _base_event(tenant_id: str, event_type: str = EVENT_TYPE_STREAM) -> dict:
    return {
        "id": str(uuid.uuid4()),
        "type": event_type,
        "tenant_id": tenant_id,
        "source": "e2e-test",
        "subject": "s1",
        "subject_type": SUBJECT_TYPE,
        "occurred_at": datetime.now(timezone.utc).isoformat(),
    }


async def test_publish_single_async_returns_202(api):
    """scenario: producer/single/1.01-positive-publish-single-async.md"""
    tenant_id = str(uuid.uuid4())
    async with api() as client:
        resp = await client.post("/events", json=_base_event(tenant_id))
    assert resp.status_code == 202
    assert resp.text == "", "202 Accepted must carry no body"


async def test_publish_sync_wait_persisted_returns_201(api):
    """scenario: producer/single/1.02-positive-publish-sync-wait-persisted.md"""
    tenant_id = str(uuid.uuid4())
    async with api() as client:
        resp = await client.post(
            "/events?wait=persisted",
            json=_base_event(tenant_id),
        )
    assert resp.status_code == 201
    assert resp.text == "", "201 Created must carry no body"


@pytest.mark.xfail(
    reason=(
        "schema validation against data_schema may not be implemented; "
        "EVENT_TYPE_STRICT requires data.strict_field"
    ),
    strict=False,
)
async def test_schema_validation_failure_returns_400(api):
    """scenario: producer/single/1.03-negative-schema-validation-failure.md

    EVENT_TYPE_STRICT is registered with a data schema that requires
    ``strict_field`` (string).  Publishing without it should fail validation.
    """
    tenant_id = str(uuid.uuid4())
    async with api() as client:
        resp = await client.post(
            "/events",
            json={
                **_base_event(tenant_id, event_type=EVENT_TYPE_STRICT),
                "data": {"missing_the_required_field": True},
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


@SKIP_LIMITS
async def test_rate_limited_returns_429(api):
    """scenario: producer/single/1.04-negative-rate-limited.md"""
    tenant_id = str(uuid.uuid4())
    async with api() as client:
        resp = None
        for _ in range(500):
            resp = await client.post("/events", json=_base_event(tenant_id))
            if resp.status_code == 429:
                break
    assert resp is not None and resp.status_code == 429
    body = resp.json()
    assert body == {
        "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.resource_exhausted.v1~",
        "title": "Resource Exhausted",
        "status": 429,
        "detail": body["detail"],
        "instance": body["instance"],
        "trace_id": body["trace_id"],
        "context": body["context"],
    }
    assert "retry-after" in {k.lower() for k in resp.headers}


async def test_readonly_partition_rejected_returns_400(api):
    """scenario: producer/single/1.05-negative-readonly-partition-rejected.md

    ``partition`` is a consumer-facing read-side field; a producer supplying it
    on publish gets a 400 Bad Request.
    """
    tenant_id = str(uuid.uuid4())
    async with api() as client:
        resp = await client.post(
            "/events",
            json={
                **_base_event(tenant_id),
                "partition": 0,  # read-only field; must be rejected
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
