"""Topic introspection scenarios (topics/1.01-1.04).

All tests are read-only.  The pre-provisioned topics injected into the
session-scoped server at startup (``TOPIC_STREAM``, ``TOPIC_LONGPOLL``,
``TOPIC_4P``, ``TOPIC_STRICT``) are visible to the list and segment endpoints.
"""

from __future__ import annotations

import pytest

from .conftest import EVENT_TYPE_STREAM, TOPIC_STREAM, TOPIC_LONGPOLL


async def test_list_topics_returns_paged_list(api):
    """scenario: topics/1.01-positive-list-topics.md"""
    async with api() as client:
        resp = await client.get("/topics")
    assert resp.status_code == 200
    body = resp.json()
    # Shape: paged list with items[] and page_info.
    assert "items" in body
    assert "page_info" in body
    page_info = body["page_info"]
    assert "limit" in page_info
    assert "next_cursor" in page_info
    # At least the pre-provisioned topics must appear.
    ids = {item["id"] for item in body["items"]}
    assert TOPIC_STREAM in ids
    assert TOPIC_LONGPOLL in ids
    # Each item has the documented fields.
    happy = next(i for i in body["items"] if i["id"] == TOPIC_STREAM)
    assert happy == {
        "id": TOPIC_STREAM,
        "description": happy["description"],  # value set by _topic_instance helper
        "retention": happy["retention"],       # null or ISO duration from config
    }


async def test_list_topic_segments_returns_manifest(api):
    """scenario: topics/1.02-positive-list-topic-segments.md"""
    async with api() as client:
        resp = await client.get(
            f"/topics/segments?topic={TOPIC_STREAM}&partition=0"
        )
    assert resp.status_code == 200
    body = resp.json()
    assert body == {
        "topic": TOPIC_STREAM,
        "partition": 0,
        "start_sequence": body["start_sequence"],
        "end_sequence": body["end_sequence"],
        "start_time": body["start_time"],
        "end_time": body["end_time"],
        "segments": body["segments"],
    }
    assert isinstance(body["segments"], list)


async def test_segments_for_unknown_topic_returns_404(api):
    """scenario: topics/1.03-negative-segments-unknown-topic.md"""
    unknown = "gts.cf.core.events.topic.v1~cf.e2e.event_broker.nonexistent.v1"
    async with api() as client:
        resp = await client.get(
            f"/topics/segments?topic={unknown}&partition=0"
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
            "resource_type": "gts.cf.core.events.topic.v1~",
            "resource_name": unknown,
        },
    }


async def test_list_event_types_returns_paged_list(api):
    """scenario: topics/1.04-positive-list-event-types.md"""
    async with api() as client:
        resp = await client.get(f"/event-types?topic={TOPIC_STREAM}")
    assert resp.status_code == 200
    body = resp.json()
    assert "items" in body
    assert "page_info" in body
    ids = {item["id"] for item in body["items"]}
    assert EVENT_TYPE_STREAM in ids
    # Each item has the documented fields.
    happy_et = next(i for i in body["items"] if i["id"] == EVENT_TYPE_STREAM)
    assert happy_et == {
        "id": EVENT_TYPE_STREAM,
        "topic": TOPIC_STREAM,
        "description": happy_et["description"],
        "allowed_subject_types": happy_et["allowed_subject_types"],
        "data_schema": happy_et["data_schema"],
    }
