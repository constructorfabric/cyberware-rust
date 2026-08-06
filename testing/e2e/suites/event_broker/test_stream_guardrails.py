"""Content-negotiation guardrails on :stream and :sse endpoints.

Two tests here carry no ``scenario:`` tag deliberately. They apply the rule the
tagged scenarios state - an endpoint rejects an Accept it cannot serve - to the
directions those scenarios do not enumerate: SSE asked for multipart, and a
wildcard Accept that must NOT be refused. They assert the documented rule rather
than a property of their own, so they are not operational tests, and tagging
them would double-claim a scenario that already has its own test.

scenarios/consumer/stream/1.07-guardrail-stream-accept-json-rejected.md
scenarios/consumer/stream/1.08-guardrail-sse-from-stream-endpoint.md

:stream serves multipart/mixed only; :sse serves text/event-stream only.
A client that sends an incompatible Accept header receives 406 Not Acceptable
before any subscription lookup is attempted — no subscription needs to exist.
"""

from __future__ import annotations

import uuid


async def test_stream_rejects_application_json_accept(api):
    """scenario: consumer/stream/1.07-guardrail-stream-accept-json-rejected.md

    GET /events:stream with Accept: application/json must return 406.
    The response body is an RFC 9457 problem document listing multipart/mixed
    as the only supported type.
    """
    sub_id = str(uuid.uuid4())
    async with api() as client:
        resp = await client.get(
            f"/events:stream?subscription_id={sub_id}",
            headers={"Accept": "application/json"},
        )

    assert resp.status_code == 406
    body = resp.json()
    assert body["status"] == 406
    assert "invalid_argument" in body["type"]
    assert body["detail"] == "this endpoint serves multipart/mixed only"
    assert body["instance"] == "/event-broker/v1/events:stream"


async def test_stream_rejects_text_event_stream_accept(api):
    """scenario: consumer/stream/1.08-guardrail-sse-from-stream-endpoint.md

    GET /events:stream with Accept: text/event-stream must return 406 and
    direct the client to /v1/events:sse instead.
    """
    sub_id = str(uuid.uuid4())
    async with api() as client:
        resp = await client.get(
            f"/events:stream?subscription_id={sub_id}",
            headers={"Accept": "text/event-stream"},
        )

    assert resp.status_code == 406
    body = resp.json()
    assert body["status"] == 406
    assert "invalid_argument" in body["type"]
    assert "/v1/events:sse" in body["detail"]
    assert body["instance"] == "/event-broker/v1/events:stream"


async def test_sse_rejects_multipart_mixed_accept(api):
    """Symmetric to scenario 1.08: GET /events:sse with Accept: multipart/mixed
    must return 406 and direct the client to /v1/events:stream instead.
    """
    sub_id = str(uuid.uuid4())
    async with api() as client:
        resp = await client.get(
            f"/events:sse?subscription_id={sub_id}",
            headers={"Accept": "multipart/mixed"},
        )

    assert resp.status_code == 406
    body = resp.json()
    assert body["status"] == 406
    assert "invalid_argument" in body["type"]
    assert "/v1/events:stream" in body["detail"]
    assert body["instance"] == "/event-broker/v1/events:sse"


async def test_stream_accepts_wildcard_accept(api):
    """Accept: */* on :stream must not return 406 — absent or wildcard Accept
    is treated as 'accept anything', consistent with HTTP content negotiation.
    The response progresses past the Accept check (404 or 409 depending on
    whether the subscription exists, never 406).
    """
    sub_id = str(uuid.uuid4())
    async with api() as client:
        resp = await client.get(
            f"/events:stream?subscription_id={sub_id}",
            headers={"Accept": "*/*"},
        )

    assert resp.status_code != 406
