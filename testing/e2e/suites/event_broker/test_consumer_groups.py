"""Consumer group CRUD scenarios (consumer/groups/1.01-1.09).

Each test creates its own group to avoid cross-test interference.
``groups/1.08`` (named group JOIN) is xfail: named groups are provisioned via
the types-registry at startup and the E2E config does not include a pre-seeded
named group entity.
``groups/1.09`` (cross-tenant ownership) tests are xfail: the standalone
config sets ``auth_disabled: true``, so tenant isolation cannot be exercised.
``groups/1.06`` validation tests (client_agent length, description encoding)
run live; the server enforces both rules.
"""

from __future__ import annotations

import uuid

import pytest

from .conftest import EVENT_TYPE_STREAM, SUBJECT_TYPE, TOPIC_STREAM, XFAIL_AUTH_DISABLED


async def test_create_anonymous_group_returns_201(api):
    """scenario: consumer/groups/1.01-positive-create-anonymous-group.md"""
    async with api() as client:
        resp = await client.post(
            "/consumer-groups",
            json={"client_agent": "e2e-test", "description": "order-fulfilment workers"},
        )
    assert resp.status_code == 201
    body = resp.json()
    assert body == {
        "id": body["id"],
        "kind": "anonymous",
        "tenant_id": body["tenant_id"],
        "owner_principal_id": body["owner_principal_id"],
        "description": "order-fulfilment workers",
        "created_at": body["created_at"],
    }
    assert body["id"].startswith("gts.cf.core.events.consumer_group.v1~")
    assert "location" in {k.lower() for k in resp.headers}


async def test_create_anonymous_group_no_body_returns_201(api):
    """scenario: consumer/groups/1.01 — the request body is optional."""
    async with api() as client:
        resp = await client.post("/consumer-groups")
    assert resp.status_code == 201
    body = resp.json()
    assert body["id"].startswith("gts.cf.core.events.consumer_group.v1~")
    assert body["kind"] == "anonymous"
    assert body["description"] is None


async def test_get_group_by_id_returns_200(api):
    """scenario: consumer/groups/1.02-positive-get-group-by-id.md"""
    async with api() as client:
        create_resp = await client.post(
            "/consumer-groups",
            json={"client_agent": "e2e-test", "description": "test group"},
        )
        assert create_resp.status_code == 201
        created = create_resp.json()
        group_id = created["id"]

        get_resp = await client.get(f"/consumer-groups/{group_id}")
    assert get_resp.status_code == 200
    assert get_resp.json() == created


async def test_list_groups_returns_paged_list(api):
    """scenario: consumer/groups/1.03-positive-list-groups.md"""
    async with api() as client:
        # Create a group so there is at least one to list.
        create_resp = await client.post(
            "/consumer-groups",
            json={"client_agent": "e2e-test", "description": "list-test group"},
        )
        assert create_resp.status_code == 201
        created = create_resp.json()
        group_id = created["id"]

        list_resp = await client.get("/consumer-groups")
    assert list_resp.status_code == 200
    body = list_resp.json()
    assert body == {
        "items": body["items"],
        "page_info": body["page_info"],
    }
    assert isinstance(body["items"], list)
    assert isinstance(body["page_info"], dict)

    # The created group must appear in the list with its full shape intact.
    found = next((item for item in body["items"] if item["id"] == group_id), None)
    assert found is not None, f"group {group_id} not found in list"
    assert found == created


async def test_delete_empty_group_returns_204(api):
    """scenario: consumer/groups/1.04-positive-delete-empty-group.md"""
    async with api() as client:
        create_resp = await client.post("/consumer-groups")
        assert create_resp.status_code == 201
        group_id = create_resp.json()["id"]

        del_resp = await client.delete(f"/consumer-groups/{group_id}")
    assert del_resp.status_code == 204
    assert del_resp.text == ""


async def test_delete_group_with_active_members_returns_409(api):
    """scenario: consumer/groups/1.05-negative-delete-group-with-active-members.md"""
    tenant_id = str(uuid.uuid4())
    async with api() as client:
        create_resp = await client.post("/consumer-groups")
        assert create_resp.status_code == 201
        group_id = create_resp.json()["id"]

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

        del_resp = await client.delete(f"/consumer-groups/{group_id}")
    assert del_resp.status_code == 409
    body = del_resp.json()
    assert body == {
        "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.failed_precondition.v1~",
        "title": "Failed Precondition",
        "status": 409,
        "detail": body["detail"],
        "instance": body["instance"],
        "trace_id": body["trace_id"],
        "context": {
            "resource_type": "gts.cf.core.events.consumer_group.v1~",
            "violations": [
                {
                    "type": "has_active_members",
                    "subject": group_id,
                    "description": body["context"]["violations"][0]["description"],
                }
            ]
        },
    }


async def test_client_agent_too_long_returns_400(api):
    """scenario: consumer/groups/1.06-negative-invalid-client-agent.md

    A ``client_agent`` of 257 bytes (one past the 256-byte cap) is rejected
    with 400.  The error names the field, states the bound and the measured
    length, and carries no part of the submitted value.
    """
    async with api() as client:
        resp = await client.post(
            "/consumer-groups",
            json={"client_agent": "x" * 257},
        )
    assert resp.status_code == 400
    body = resp.json()
    assert body == {
        "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~",
        "title": "Invalid Argument",
        "status": 400,
        "detail": "Request validation failed",
        "instance": body["instance"],
        "trace_id": body["trace_id"],
        "context": {
            "field_violations": [
                {
                    "field": "client_agent",
                    "description": "must be 1-256 bytes, got 257",
                    "reason": "field_too_long",
                }
            ],
            "resource_type": "gts.cf.core.events.request.v1~",
        },
    }


async def test_description_non_ascii_returns_400(api):
    """scenario: consumer/groups/1.06-negative-invalid-client-agent.md

    A ``description`` carrying bytes outside the printable-ASCII range
    (0x20-0x7E) is rejected with 400.  The error names the field and the
    ascii_only rule; it does not reproduce the submitted value.
    """
    async with api() as client:
        resp = await client.post(
            "/consumer-groups",
            json={"client_agent": "e2e-test", "description": "é\x01"},
        )
    assert resp.status_code == 400
    body = resp.json()
    assert body == {
        "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~",
        "title": "Invalid Argument",
        "status": 400,
        "detail": "Request validation failed",
        "instance": body["instance"],
        "trace_id": body["trace_id"],
        "context": {
            "field_violations": [
                {
                    "field": "description",
                    "description": "must contain only printable ASCII (0x20-0x7E)",
                    "reason": "ascii_only",
                }
            ],
            "resource_type": "gts.cf.core.events.request.v1~",
        },
    }


async def test_get_unknown_group_returns_404(api):
    """scenario: consumer/groups/1.07-negative-get-unknown-group.md"""
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


@pytest.mark.xfail(
    reason=(
        "named groups require a types-registry entity; "
        "no named group is pre-seeded in the E2E standalone config"
    ),
    strict=False,
)
async def test_named_group_join_without_create_step(api):
    """scenario: consumer/groups/1.08-positive-named-group-join.md

    A named consumer group has a well-known GTS identifier registered via
    types_registry at startup.  The consumer JOINs directly without a prior
    POST /consumer-groups.  This test is xfail because no named group entity is
    included in the E2E standalone config.
    """
    named_group_id = "gts.cf.core.events.consumer_group.v1~cf.e2e.event_broker.named_group.v1"
    tenant_id = str(uuid.uuid4())
    async with api() as client:
        sub_resp = await client.post(
            "/subscriptions",
            json={
                "consumer_group": named_group_id,
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


@XFAIL_AUTH_DISABLED
async def test_cross_tenant_get_anonymous_group_returns_403(api):
    """scenario: consumer/groups/1.09-negative-cross-tenant-anonymous-group-ownership.md

    Tenant A creates an anonymous group; tenant B's GET on that group must
    return 403, not 200.  In standalone mode auth is disabled so the server
    returns 200 — hence strict xfail.
    """
    async with api() as client:
        create_resp = await client.post("/consumer-groups")
        assert create_resp.status_code == 201
        group_id = create_resp.json()["id"]

        # In a real deployment this request would carry a different tenant's
        # bearer token.  Standalone auth is disabled so this is indistinguishable
        # from the owning tenant — the test is here to document the contract.
        get_resp = await client.get(f"/consumer-groups/{group_id}")
    assert get_resp.status_code == 403
    body = get_resp.json()
    assert body == {
        "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.permission_denied.v1~",
        "title": "Permission Denied",
        "status": 403,
        "detail": body["detail"],
        "instance": body["instance"],
        "trace_id": body["trace_id"],
        "context": body["context"],
    }


@XFAIL_AUTH_DISABLED
async def test_cross_tenant_delete_anonymous_group_returns_403(api):
    """scenario: consumer/groups/1.09-negative-cross-tenant-anonymous-group-ownership.md

    Tenant A creates an anonymous group; tenant B's DELETE on that group must
    return 403 and leave the group intact.
    """
    async with api() as client:
        create_resp = await client.post("/consumer-groups")
        assert create_resp.status_code == 201
        group_id = create_resp.json()["id"]

        del_resp = await client.delete(f"/consumer-groups/{group_id}")
    assert del_resp.status_code == 403
    body = del_resp.json()
    assert body == {
        "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.permission_denied.v1~",
        "title": "Permission Denied",
        "status": 403,
        "detail": body["detail"],
        "instance": body["instance"],
        "trace_id": body["trace_id"],
        "context": body["context"],
    }


@XFAIL_AUTH_DISABLED
async def test_list_excludes_other_tenant_anonymous_groups(api):
    """scenario: consumer/groups/1.09-negative-cross-tenant-anonymous-group-ownership.md

    Tenant A's anonymous groups must not appear in tenant B's group listing.
    """
    async with api() as client:
        create_resp = await client.post("/consumer-groups")
        assert create_resp.status_code == 201
        group_id = create_resp.json()["id"]

        # Listing as a different tenant: the group created above must be absent.
        list_resp = await client.get("/consumer-groups")
    assert list_resp.status_code == 200
    items = list_resp.json()["items"]
    assert not any(item["id"] == group_id for item in items)
