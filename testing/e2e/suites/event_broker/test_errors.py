"""RFC-9457 error envelope scenarios (errors/1.01-1.08).

Each test triggers one canonical error category and asserts the full
Problem Details shape (``type``, ``title``, ``status``, ``detail``,
``instance``, ``trace_id``, ``context``).  Server-generated fields
(``detail``, ``instance``, ``trace_id``) are self-referenced so the
assertion covers the whole envelope without hard-coding their exact text.

Unauthenticated (1.02) and PermissionDenied (1.03) are xfail because the
standalone config has ``auth_disabled: true``.  The 500 Internal (1.08) test
is xfail because triggering a genuine internal error requires deliberate fault
injection not available in the E2E harness.
"""

from __future__ import annotations

import uuid
from datetime import datetime, timezone

import pytest

from .conftest import SKIP_LIMITS, EVENT_TYPE_STREAM, SUBJECT_TYPE, TOPIC_STREAM

_AUTH_DISABLED_REASON = (
    "standalone config sets auth_disabled: true; "
    "bearer-token validation is not active"
)


async def test_problem_details_envelope_shape(api):
    """scenario: errors/1.01-positive-problem-details-envelope.md

    A representative 404 verifies that every error carries the canonical RFC-9457
    + GTS envelope: ``type``, ``title``, ``status``, ``detail``, ``instance``,
    ``trace_id``, and ``context``.
    """
    fake_id = "gts.cf.core.events.consumer_group.v1~00000000-0000-0000-0000-000000000000"
    async with api() as client:
        resp = await client.get(f"/consumer-groups/{fake_id}")
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
            "resource_type": "gts.cf.core.events.consumer_group.v1~",
            "resource_name": fake_id,
        },
    }
    assert resp.headers.get("content-type", "").startswith("application/problem+json")


@pytest.mark.xfail(reason=_AUTH_DISABLED_REASON, strict=False)
async def test_401_unauthenticated_envelope(api):
    """scenario: errors/1.02-negative-401-unauthenticated.md"""
    async with api() as client:
        resp = await client.get("/consumer-groups", headers={"Authorization": ""})
    assert resp.status_code == 401
    body = resp.json()
    assert body == {
        "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.unauthenticated.v1~",
        "title": "Unauthenticated",
        "status": 401,
        "detail": body["detail"],
        "instance": body["instance"],
        "trace_id": body["trace_id"],
        "context": body["context"],
    }


@pytest.mark.xfail(reason=_AUTH_DISABLED_REASON, strict=False)
async def test_403_permission_denied_envelope(api):
    """scenario: errors/1.03-negative-403-unauthorized.md"""
    tenant_id = str(uuid.uuid4())
    async with api() as client:
        resp = await client.post(
            "/events",
            json={
                "id": str(uuid.uuid4()),
                "type": EVENT_TYPE_STREAM,
                "tenant_id": tenant_id,
                "source": "e2e-test",
                "subject": "s1",
                "subject_type": SUBJECT_TYPE,
                "occurred_at": datetime.now(timezone.utc).isoformat(),
            },
        )
    assert resp.status_code == 403
    body = resp.json()
    assert body == {
        "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.permission_denied.v1~",
        "title": "Permission Denied",
        "status": 403,
        "detail": body["detail"],
        "instance": body["instance"],
        "trace_id": body["trace_id"],
        "context": body["context"],
    }


async def test_404_not_found_envelope(api):
    """scenario: errors/1.04-negative-404-not-found.md

    GET on a non-existent consumer group is the reference 404 case from the
    scenario file.
    """
    fake_id = "gts.cf.core.events.consumer_group.v1~deadbeef-0000-0000-0000-000000000000"
    async with api() as client:
        resp = await client.get(f"/consumer-groups/{fake_id}")
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
            "resource_type": "gts.cf.core.events.consumer_group.v1~",
            "resource_name": fake_id,
        },
    }


async def test_409_failed_precondition_envelope(api):
    """scenario: errors/1.05-negative-409-conflict.md

    Opening a stream before SEEKing produces a 409 PositionsNotSet, which is
    the canonical FailedPrecondition error.  The scenario file's status field
    reflects the implementation's 409 HTTP override (the category default is
    400 but ``error.rs`` applies ``Http::status_code(409)`` for this code).
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
                        "topic": TOPIC_STREAM,
                        "tenant_id": tenant_id,
                        "types": [EVENT_TYPE_STREAM],
                    }
                ],
            },
        )
        assert sub_resp.status_code == 201
        sub_id = sub_resp.json()["id"]

        # Stream without a prior SEEK → PositionsNotSet.
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


async def test_412_sequence_violation_envelope(api):
    """scenario: errors/1.06-negative-412-sequence-violation.md

    A chained producer whose ``meta.previous`` does not match the broker's
    stored ``last_sequence`` gets a 412.
    """
    async with api() as client:
        reg_resp = await client.post(
            "/producers",
            json={"mode": "chained", "client_agent": "e2e-test"},
        )
        assert reg_resp.status_code == 201
        producer_id = reg_resp.json()["id"]

        tenant_id = str(uuid.uuid4())
        # Publish first event normally (no meta → monotonic, or just let it work).
        first_resp = await client.post(
            "/events",
            headers={"Producer-Id": producer_id},
            json={
                "id": str(uuid.uuid4()),
                "type": EVENT_TYPE_STREAM,
                "tenant_id": tenant_id,
                "source": "e2e-test",
                "subject": "s1",
                "subject_type": SUBJECT_TYPE,
                "occurred_at": datetime.now(timezone.utc).isoformat(),
                "meta": {
                    "version": 1,
                    "producer_id": producer_id,
                    "previous": 0,
                    "sequence": 1,
                },
            },
        )
        assert first_resp.status_code == 202

        # Now publish with wrong meta.previous (stale / mismatched).
        resp = await client.post(
            "/events",
            headers={"Producer-Id": producer_id},
            json={
                "id": str(uuid.uuid4()),
                "type": EVENT_TYPE_STREAM,
                "tenant_id": tenant_id,
                "source": "e2e-test",
                "subject": "s1",
                "subject_type": SUBJECT_TYPE,
                "occurred_at": datetime.now(timezone.utc).isoformat(),
                "meta": {
                    "version": 1,
                    "producer_id": producer_id,
                    "previous": 999,  # wrong — broker has last_sequence=1
                    "sequence": 1000,
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


@SKIP_LIMITS
async def test_429_rate_limited_envelope(api):
    """scenario: errors/1.07-negative-429-rate-limited.md"""
    tenant_id = str(uuid.uuid4())
    async with api() as client:
        resp = None
        # Flood until we get 429 or exhaust attempts.
        for _ in range(200):
            resp = await client.post(
                "/events",
                json={
                    "id": str(uuid.uuid4()),
                    "type": EVENT_TYPE_STREAM,
                    "tenant_id": tenant_id,
                    "source": "e2e-test",
                    "subject": "s1",
                    "subject_type": SUBJECT_TYPE,
                    "occurred_at": datetime.now(timezone.utc).isoformat(),
                },
            )
            if resp.status_code == 429:
                break
    assert resp is not None
    assert resp.status_code == 429
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


@pytest.mark.xfail(
    reason="triggering a genuine 500 requires fault injection not available in E2E",
    strict=False,
)
async def test_500_internal_error_envelope(api):
    """scenario: errors/1.08-negative-500-internal.md"""
    # No reliable way to force an internal error from the outside in E2E;
    # this test documents the expected shape for when one does occur.
    async with api() as client:
        resp = await client.get("/admin/force-internal-error")  # hypothetical endpoint
    assert resp.status_code == 500
    body = resp.json()
    assert body == {
        "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.internal.v1~",
        "title": "Internal",
        "status": 500,
        "detail": body["detail"],
        "instance": body["instance"],
        "trace_id": body["trace_id"],
        "context": body["context"],
    }
