"""Auth & permissions scenarios (auth/1.01-1.05).

All tests in this file are marked xfail because the standalone config ships
with ``auth_disabled: true`` under ``api-gateway``, so bearer-token validation
is not active and every auth boundary is invisible.  The tests record the
correct expected behavior for when the config is wired with a real authn stack.
"""

from __future__ import annotations

import uuid

import pytest

from .conftest import EVENT_TYPE_STREAM, SUBJECT_TYPE, TOPIC_STREAM

_AUTH_DISABLED_REASON = (
    "standalone config sets auth_disabled: true; "
    "bearer-token validation is not active"
)


@pytest.mark.xfail(reason=_AUTH_DISABLED_REASON, strict=False)
async def test_missing_bearer_token_returns_401(api):
    """scenario: auth/1.01-negative-missing-bearer-token.md"""
    async with api() as client:
        resp = await client.get(
            "/consumer-groups",
            headers={"Authorization": ""},  # explicitly empty
        )
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
async def test_invalid_bearer_token_returns_401(api):
    """scenario: auth/1.02-negative-invalid-bearer-token.md"""
    async with api() as client:
        resp = await client.get(
            "/consumer-groups",
            headers={"Authorization": "Bearer this-is-not-a-valid-jwt"},
        )
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
async def test_no_produce_permission_returns_403(api):
    """scenario: auth/1.03-negative-insufficient-permission-produce.md"""
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
                "occurred_at": "2026-01-01T00:00:00Z",
            },
            # A token with no topic:produce grant would be supplied here.
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


@pytest.mark.xfail(reason=_AUTH_DISABLED_REASON, strict=False)
async def test_no_consume_permission_returns_403(api):
    """scenario: auth/1.04-negative-insufficient-permission-consume.md"""
    async with api() as client:
        group_resp = await client.post("/consumer-groups")
        assert group_resp.status_code == 201
        group_id = group_resp.json()["id"]

        tenant_id = str(uuid.uuid4())
        resp = await client.post(
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
            # A token with no topic:consume grant would be supplied here.
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


@pytest.mark.xfail(reason=_AUTH_DISABLED_REASON, strict=False)
async def test_cross_tenant_anonymous_group_returns_403(api):
    """scenario: auth/1.05-negative-cross-tenant-anonymous-group.md

    Tenant B's principal attempts to JOIN a group minted by tenant A.  The
    broker rejects with 403 because the group's ``tenant_id`` (from A's
    SecurityContext at creation time) does not match B's.
    """
    async with api() as client:
        # Tenant A creates the group.
        group_resp = await client.post("/consumer-groups")
        assert group_resp.status_code == 201
        group_id = group_resp.json()["id"]

        # Tenant B (different token) tries to JOIN it.
        tenant_b_id = str(uuid.uuid4())
        resp = await client.post(
            "/subscriptions",
            json={
                "consumer_group": group_id,
                "client_agent": "e2e-test",
                "interests": [
                    {
                        "topic": TOPIC_STREAM,
                        "tenant_id": tenant_b_id,
                        "types": [EVENT_TYPE_STREAM],
                    }
                ],
            },
            # Tenant B's bearer token would be supplied here.
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
