"""E2E fixtures for the event-broker gear (design.md D8, task group 11).

event-broker has its OWN standalone binary (`cf-gears-event-broker-server`),
not a feature flag on `cf-gears-example-server` - gated on `E2E_BINARY`
exactly like mini-chat/usage-collector, so running the shared `make
e2e-local` suite never boots a second server for routes it doesn't serve.

Run it with: make e2e-event-broker

`types-registry`'s config-seeded `entities:` are committed to queryable
storage once, at boot (`post_init`), with no live-refresh path (see
`config/event-broker-standalone.yaml`'s own header comment) - registering a
NEW topic at runtime via `POST /types-registry/v1/entities` would NOT become
visible to event-broker's own specification cache without a full restart.
So every test that shares the session-scoped `test_env` server gets its own
PRE-PROVISIONED topic baked into the config at session-setup time (below),
rather than registering one for itself at test time - there is no other way
to get per-test topic isolation against a server that boots once for the
whole session.
"""

from __future__ import annotations

import asyncio
import json
import os
import tempfile
import uuid
from datetime import datetime, timezone
from pathlib import Path

import httpx
import pytest

from lib.orchestrator import GearTestEnv

# ── Constants ─────────────────────────────────────────────────────────────

HERE = Path(__file__).resolve().parent
PROJECT_ROOT = Path(__file__).resolve().parents[4]
CONFIG = PROJECT_ROOT / "config" / "event-broker-standalone.yaml"

SERVER_PORT = 8089
API_BASE = "/event-broker/v1"
REQUEST_TIMEOUT = 5.0

SUBJECT_TYPE = "gts.cf.e2e.event_broker.subject.v1~"

# Active streaming topic: tests publish events here and verify delivery via
# the SSE or multipart stream. Each test generates a unique tenant_id so
# events from different tests never mix in the same subscription filter.
# A fresh subscription replays the topic's whole history from offset 0
# (`domain/delivery.rs`'s `stream()` seeds the cursor from the join-time
# `assigned.offset`, not from whatever seek() last persisted), so tests must
# use a unique tenant_id to isolate their own events from prior test runs.
TOPIC_STREAM = "gts.cf.core.events.topic.v1~cf.e2e.event_broker.stream.v1"
EVENT_TYPE_STREAM = "gts.cf.core.events.event.v1~cf.e2e.event_broker.stream.v1~"

# Long-poll topic: dedicated to heartbeat and wake-on-publish timing tests.
# Kept separate so its quiet periods don't race with TOPIC_STREAM's traffic.
TOPIC_LONGPOLL = "gts.cf.core.events.topic.v1~cf.e2e.event_broker.longpoll.v1"
EVENT_TYPE_LONGPOLL = "gts.cf.core.events.event.v1~cf.e2e.event_broker.longpoll.v1~"

# 4-partition topic for rebalance tests (subscriptions/1.11, 1.12, 1.13;
# consumer/flows/1.01): needs enough partitions to split across multiple members.
TOPIC_4P = "gts.cf.core.events.topic.v1~cf.e2e.event_broker.four_part.v1"
EVENT_TYPE_4P = "gts.cf.core.events.event.v1~cf.e2e.event_broker.four_part.v1~"

# Strict-schema topic for schema-validation tests (producer/single/1.03):
# its event type requires data.strict_field as a string, so publishing
# without it triggers a 400 schema-validation failure.
TOPIC_STRICT = "gts.cf.core.events.topic.v1~cf.e2e.event_broker.strict.v1"
EVENT_TYPE_STRICT = "gts.cf.core.events.event.v1~cf.e2e.event_broker.strict.v1~"

# Read-only topic for seek/cursor tests: seeded with PREFILLED_COUNT events
# at session bootstrap via the `prefilled_topic` fixture; no test ever
# publishes here. Because the database is fresh each session, sequences start
# at 1 and the cursor values (earliest=0, latest=PREFILLED_COUNT) are
# deterministic constants that tests can assert directly.
TOPIC_PREFILLED = "gts.cf.core.events.topic.v1~cf.e2e.event_broker.prefilled.v1"
EVENT_TYPE_PREFILLED = "gts.cf.core.events.event.v1~cf.e2e.event_broker.prefilled.v1~"
PREFILLED_TENANT_ID = "e2e00000-0000-0000-0000-000000000000"
PREFILLED_COUNT = 100
# How long the prefill fixture waits for its batch to become durable.
PREFILL_INGEST_TIMEOUT = 30.0

# Shared xfail marker for tests that require real auth enforcement.
# The standalone config sets ``auth_disabled: true``; these tests document the
# expected behaviour and will pass once a multi-tenant config is wired.
XFAIL_AUTH_DISABLED = pytest.mark.xfail(
    reason="standalone config sets auth_disabled: true; tenant isolation is not enforced",
    strict=True,
)

# Quotas and caps - rate limits, consumer-group capacity, interest counts - are
# out of scope for now. The tests that pin their rejections are parked until
# those limits are implemented and made configurable.
SKIP_LIMITS = pytest.mark.skip(reason="limits are not implemented yet")

# One partition per topic: every test here asserts a single-partition
# assignment, and a topic carries no partition count of its own - it is
# `event-broker` configuration, written into the generated config below.
_TOPIC_PARTITIONS = {
    TOPIC_STREAM: 1,
    TOPIC_LONGPOLL: 1,
    TOPIC_4P: 4,
    TOPIC_STRICT: 1,
    TOPIC_PREFILLED: 1,
}


def _topic_instance(topic_id: str) -> dict:
    """A topic as `types-registry` holds it: an instance of the topic base."""
    return {"id": topic_id, "description": "a topic this E2E suite publishes to"}


def _derived_event_type(type_id: str, topic_id: str) -> dict:
    """An event type as `types-registry` holds it: a derived *type schema*.

    Its traits are what govern the type - the topic its events land on, the
    subject types they may declare, and the member of an event that decides its
    partition. `data` narrows to the base's own `["object", "null"]`: an empty
    schema would be *wider* than the base, and registration refuses a derived
    type that widens.
    """
    return {
        "$id": f"gts://{type_id}",
        "$schema": "http://json-schema.org/draft-07/schema#",
        "x-gts-traits": {
            "topic": topic_id,
            "allowed_subject_types": [SUBJECT_TYPE],
            "partition_key": "/tenant_id",
        },
        "type": "object",
        "allOf": [
            {"$ref": "gts://gts.cf.core.events.event.v1~"},
            {"type": "object", "properties": {"data": {"type": ["object", "null"]}}},
        ],
    }


def _strict_event_type(type_id: str, topic_id: str) -> dict:
    """An event type that requires ``data.strict_field`` (string).

    Used by schema-validation tests to trigger a 400 when the field is absent.
    The constraint is NARROWER than the base (which allows any object or null),
    so registration should accept it.
    """
    return {
        "$id": f"gts://{type_id}",
        "$schema": "http://json-schema.org/draft-07/schema#",
        "x-gts-traits": {
            "topic": topic_id,
            "allowed_subject_types": [SUBJECT_TYPE],
            "partition_key": "/tenant_id",
        },
        "type": "object",
        "allOf": [
            {"$ref": "gts://gts.cf.core.events.event.v1~"},
            {
                "type": "object",
                "properties": {
                    "data": {
                        "type": "object",
                        "required": ["strict_field"],
                        "properties": {"strict_field": {"type": "string"}},
                    }
                },
            },
        ],
    }


_EXTRA_ENTITIES = [
    _topic_instance(TOPIC_STREAM),
    _derived_event_type(EVENT_TYPE_STREAM, TOPIC_STREAM),
    _topic_instance(TOPIC_LONGPOLL),
    _derived_event_type(EVENT_TYPE_LONGPOLL, TOPIC_LONGPOLL),
    _topic_instance(TOPIC_4P),
    _derived_event_type(EVENT_TYPE_4P, TOPIC_4P),
    _topic_instance(TOPIC_STRICT),
    _strict_event_type(EVENT_TYPE_STRICT, TOPIC_STRICT),
    _topic_instance(TOPIC_PREFILLED),
    _derived_event_type(EVENT_TYPE_PREFILLED, TOPIC_PREFILLED),
]

_TEMP_HOME = tempfile.mkdtemp(prefix="cf-gears-e2e-event-broker-")


# ── Environment gate ──────────────────────────────────────────────────────

@pytest.fixture(scope="session", autouse=True)
def _require_dedicated_binary():
    if not os.environ.get("E2E_BINARY"):
        pytest.skip(
            "E2E_BINARY not set — run these tests via: make e2e-event-broker",
            allow_module_level=True,
        )


def pytest_collection_modifyitems(items):
    """Exclude this suite's own session-fixture (server boot) startup cost
    from pytest.ini's global 10s per-test timeout - matches
    `usage_collector/conftest.py`'s identical hook and its own doc comment
    for the full rationale (`func_only=True` bounds only the test body, not
    fixture setup; both sides resolve their paths for the same reason).
    """
    for item in items:
        if HERE not in Path(str(item.fspath)).resolve().parents:
            continue
        if item.get_closest_marker("timeout") is None:
            item.add_marker(pytest.mark.timeout(func_only=True))


# ── Test environment ──────────────────────────────────────────────────────

def _patch_config(config_text: str, env: GearTestEnv) -> str:
    config_text = config_text.replace(
        'home_dir: "~/.cf-gears-event-broker"',
        f'home_dir: "{_TEMP_HOME}"',
    )
    # The storage backend opens its own database, and the committed config
    # points it at the real home directory. Redirect it into this session's
    # temp home too, or every run of this suite would append to one event log
    # shared with the last run and with the restart test's own server.
    config_text = config_text.replace(
        'path: "~/.cf-gears-event-broker/event-broker/event_log.db"',
        f'path: "{_TEMP_HOME}/event-broker/event_log.db"',
    )
    config_text = config_text.replace(
        'bind_addr: "127.0.0.1:8080"',
        f'bind_addr: "127.0.0.1:{SERVER_PORT}"',
    )
    # Each topic's partition count is configuration, not part of the entity.
    settings = "".join(
        f'        "{topic}":\n          partitions: {count}\n'
        for topic, count in _TOPIC_PARTITIONS.items()
    )
    config_text = config_text.replace("      topics:\n", "      topics:\n" + settings, 1)
    extra = "".join(f"        - {json.dumps(e)}\n" for e in _EXTRA_ENTITIES)
    return config_text.replace("entities:\n", "entities:\n" + extra)


@pytest.fixture(scope="session")
def gear_test_env() -> GearTestEnv:
    return GearTestEnv(
        binary="cf-gears-event-broker-server",
        config_path=CONFIG,
        config_patch=_patch_config,
        port=SERVER_PORT,
        health_path="/healthz",
        health_timeout=30,
        env={"RUST_LOG": os.environ.get("RUST_LOG", "info,event_broker=debug")},
        log_suffix="event-broker",
    )


# ── HTTP helpers ──────────────────────────────────────────────────────────

@pytest.fixture
def api(test_env):
    """Async client factory bound to the running server's event-broker API."""

    def _client() -> httpx.AsyncClient:
        return httpx.AsyncClient(
            base_url=f"{test_env.base_url}{API_BASE}",
            timeout=REQUEST_TIMEOUT,
        )

    return _client


@pytest.fixture(scope="session")
async def prefilled_topic(test_env):
    """Seed TOPIC_PREFILLED with PREFILLED_COUNT events at session startup.

    No other test publishes to TOPIC_PREFILLED. The database is fresh each
    session, so sequences start at 1 and seek("latest") after this fixture
    returns exactly PREFILLED_COUNT. Tests assert that constant directly.
    """
    base_url = f"{test_env.base_url}{API_BASE}"
    async with httpx.AsyncClient(base_url=base_url, timeout=REQUEST_TIMEOUT) as client:
        now = datetime.now(timezone.utc).isoformat()
        pub_resp = await client.post(
            "/events:batch",
            json={
                "events": [
                    {
                        "id": str(uuid.uuid4()),
                        "type": EVENT_TYPE_PREFILLED,
                        "tenant_id": PREFILLED_TENANT_ID,
                        "source": "e2e-prefill",
                        "subject": "prefill-subject",
                        "subject_type": SUBJECT_TYPE,
                        "occurred_at": now,
                    }
                    for _ in range(PREFILLED_COUNT)
                ]
            },
        )
        assert pub_resp.status_code == 202

        # `POST /events:batch` acks with 202 before the events are sequenced,
        # so the partition's HWM climbs asynchronously after this call returns.
        # Every cursor a test asserts is derived from that HWM, so the fixture
        # has to wait for all PREFILLED_COUNT to land - otherwise seek("latest")
        # reports a partially-ingested value and every bound shifts with it.
        # `GET /topics/segments` reports the partition's end_sequence, which is
        # the HWM. Temporary: a synchronous publish that acks only once durable
        # removes the need to poll, but is not implemented yet.
        deadline = asyncio.get_running_loop().time() + PREFILL_INGEST_TIMEOUT
        while True:
            seg_resp = await client.get(
                f"/topics/segments?topic={TOPIC_PREFILLED}&partition=0"
            )
            assert seg_resp.status_code == 200
            sequenced = seg_resp.json()["end_sequence"]
            if sequenced == PREFILLED_COUNT:
                break
            remaining = deadline - asyncio.get_running_loop().time()
            assert remaining > 0, (
                f"only {sequenced} of {PREFILLED_COUNT} prefill events were "
                f"sequenced within {PREFILL_INGEST_TIMEOUT}s"
            )
            await asyncio.sleep(0.1)


class FrameReader:
    """Shared frame-waiting logic for the two streaming readers below.

    A subclass supplies ``next_frame`` plus the ``_kind``/``_body`` accessors
    that say where a frame's kind and its assertable body sit in whatever shape
    that reader returns - the two transports differ there, the waiting does not.
    """

    async def await_kind(self, kind: str, timeout: float = 15.0):
        """Return the body of the next ``kind`` frame, skipping heartbeats.

        A single budget spans the whole wait. Heartbeats arrive on a few-second
        cadence, so passing ``timeout`` to each read separately would let every
        heartbeat re-arm it and a stream that never delivers ``kind`` would hang
        until the test-level timeout instead of failing here.
        """
        deadline = asyncio.get_running_loop().time() + timeout
        while True:
            remaining = deadline - asyncio.get_running_loop().time()
            assert remaining > 0, f"no {kind} frame delivered, only heartbeats"
            frame = await self.next_frame(timeout=remaining)
            if self._kind(frame) == kind:
                return self._body(frame)
            assert self._kind(frame) == "heartbeat", frame


class MultipartReader(FrameReader):
    """Reads JSON frames from a ``multipart/mixed`` streaming response.

    Each part carries exactly one JSON object, so a frame is emitted as soon as
    its body decodes - NOT when the boundary that opens the next part arrives.
    A streaming server writes that boundary only once it has another frame to
    send, so keying on it would withhold every frame until the following one
    started arriving, leaving the reader a full frame behind the stream.

    Boundaries and part headers carry no ``{``, so scanning for the next JSON
    object is enough to skip them.
    """

    def __init__(self, response: httpx.Response):
        self._iter = response.aiter_bytes()
        self._buf = b""
        self._decoder = json.JSONDecoder()

    # A multipart frame carries `kind` inline, so the frame *is* the body.
    @staticmethod
    def _kind(frame: dict) -> str:
        return frame.get("kind")

    @staticmethod
    def _body(frame: dict) -> dict:
        return frame

    async def next_frame(self, timeout: float = 5.0) -> dict:
        async def _read():
            while True:
                start = self._buf.find(b"{")
                if start != -1:
                    try:
                        text = self._buf[start:].decode("utf-8")
                        frame, end = self._decoder.raw_decode(text)
                    except (UnicodeDecodeError, ValueError):
                        # Body is still partial - fall through and read more.
                        pass
                    else:
                        # `end` indexes characters; re-encode to advance the
                        # byte buffer correctly past any non-ASCII payload.
                        self._buf = self._buf[start + len(text[:end].encode("utf-8")):]
                        return frame
                chunk = await self._iter.__anext__()
                self._buf += chunk

        return await asyncio.wait_for(_read(), timeout=timeout)


class SseFrameReader(FrameReader):
    """Reads SSE frames (`event: <kind>\\ndata: <json>\\n\\n`) one at a time
    off a streaming `httpx.Response`, buffering across chunks - a real
    socket gives no guarantee that one frame arrives as exactly one
    `aiter_bytes()` read, or that two frames don't arrive in the same one.
    """

    def __init__(self, response: httpx.Response):
        self._iter = response.aiter_bytes()
        self._buf = ""

    # SSE splits the kind onto its own `event:` line, so a frame is a pair.
    @staticmethod
    def _kind(frame: tuple[str, dict]) -> str:
        return frame[0]

    @staticmethod
    def _body(frame: tuple[str, dict]) -> dict:
        return frame[1]

    async def next_frame(self, timeout: float = 5.0) -> tuple[str, dict]:
        async def _read():
            while True:
                idx = self._buf.find("\n\n")
                if idx != -1:
                    raw, self._buf = self._buf[:idx], self._buf[idx + 2 :]
                    assert raw.startswith("event: "), f"malformed SSE block: {raw!r}"
                    event, _, data = raw[len("event: ") :].partition("\ndata: ")
                    return event, json.loads(data)
                chunk = await self._iter.__anext__()
                self._buf += chunk.decode("utf-8")

        return await asyncio.wait_for(_read(), timeout=timeout)
