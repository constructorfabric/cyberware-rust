"""Restart/durability e2e test at the process level (design.md D8, task 11.4).

Boots its OWN private server on a dedicated port, bypassing the shared
`test_env` session fixture entirely, so it can stop and restart the binary
mid-test without disrupting the other tests in this suite that depend on
the shared server for the rest of their run - matches
`gears/file_storage/lifecycle/conftest.py`'s established precedent for
bespoke process control, reusing `lib.orchestrator`'s private
`_prepare_config`/`_wait_healthy`/`_log_path` helpers directly rather than
duplicating their logic, and never touching the orchestrator's own
module-global `_server_proc`.
"""

from __future__ import annotations

import json
import os
import subprocess
import tempfile
import uuid
from datetime import datetime, timezone
from pathlib import Path

import httpx
import pytest

from lib.orchestrator import GearTestEnv, _log_path, _prepare_config, _wait_healthy

from .conftest import (
    CONFIG,
    PROJECT_ROOT,
    SUBJECT_TYPE,
    SseFrameReader,
    _derived_event_type,
    _topic_instance,
)

RESTART_PORT = 8090
TOPIC = "gts.cf.core.events.topic.v1~cf.e2e.event_broker.restart.v1"
EVENT_TYPE = "gts.cf.core.events.event.v1~cf.e2e.event_broker.restart.v1~"


def _patch_restart_config(config_text: str, home_dir: str) -> str:
    config_text = config_text.replace(
        'home_dir: "~/.cf-gears-event-broker"', f'home_dir: "{home_dir}"'
    )
    # Two databases now, and the premise of this test is that they behave
    # differently across a restart. Consumer groups and cursors survive because
    # they live in the gear database the platform provisions under `home_dir`;
    # the events survive because the storage backend's own database is a file
    # under the same directory rather than `:memory:`. Both are redirected here
    # so a restart re-opens this test's own copies of them.
    patched = config_text.replace(
        'path: "~/.cf-gears-event-broker/event-broker/event_log.db"',
        f'path: "{home_dir}/event-broker/event_log.db"',
    )
    # Asserted rather than assumed: this test proves events survive a restart,
    # and they only do because the backend's database is a file. If the
    # committed config ever stopped naming a path - `:memory:` is the default -
    # the premise would quietly become false and the test would still pass for
    # the wrong reason on the consumer-group half alone.
    assert patched != config_text, (
        "the committed config no longer names a file-backed event log, which is "
        "the premise this test rests on"
    )
    config_text = patched
    config_text = config_text.replace(
        'bind_addr: "127.0.0.1:8080"', f'bind_addr: "127.0.0.1:{RESTART_PORT}"'
    )
    entities = [
        _topic_instance(TOPIC),
        _derived_event_type(EVENT_TYPE, TOPIC),
    ]
    # The partition count is configuration, not part of the topic entity.
    config_text = config_text.replace(
        "      topics:\n",
        f'      topics:\n        "{TOPIC}":\n          partitions: 1\n',
        1,
    )
    extra = "".join(f"        - {json.dumps(e)}\n" for e in entities)
    return config_text.replace("entities:\n", "entities:\n" + extra)


def _resolve_binary() -> Path:
    binary_str = os.environ.get("E2E_BINARY")
    if not binary_str:
        pytest.fail("E2E_BINARY not set — run these tests via: make e2e-event-broker")
    p = Path(binary_str)
    if not p.exists():
        pytest.fail(f"E2E_BINARY={binary_str!r} does not exist")
    return p


def _start(home_dir: str) -> subprocess.Popen:
    env = GearTestEnv(
        config_path=CONFIG,
        config_patch=lambda text, _env: _patch_restart_config(text, home_dir),
        port=RESTART_PORT,
        health_path="/healthz",
        health_timeout=30,
        log_suffix="event-broker-restart",
    )
    config_path = _prepare_config(env)
    binary = _resolve_binary()
    log_fh = open(_log_path(env), "a")
    proc = subprocess.Popen(
        [str(binary), "--config", str(config_path), "run"],
        cwd=str(PROJECT_ROOT),
        stdout=log_fh,
        stderr=subprocess.STDOUT,
        env={**os.environ},
    )
    _wait_healthy(env)
    return proc


def _stop(proc: subprocess.Popen) -> None:
    proc.terminate()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=3)


@pytest.mark.timeout(60, func_only=True)
async def test_restart_preserves_events_and_consumer_groups_but_not_subscriptions():
    # `_require_dedicated_binary` (conftest.py, autouse) already guarantees
    # E2E_BINARY is set for every test in this directory.
    home_dir = tempfile.mkdtemp(prefix="cf-gears-e2e-event-broker-restart-")
    base_url = f"http://localhost:{RESTART_PORT}/event-broker/v1"

    proc = _start(home_dir)
    try:
        async with httpx.AsyncClient(base_url=base_url, timeout=5.0) as client:
            group_resp = await client.post("/consumer-groups")
            assert group_resp.status_code == 201
            group_body_before = group_resp.json()
            group_id = group_body_before["id"]

            tenant_id = str(uuid.uuid4())
            sub1_resp = await client.post(
                "/subscriptions",
                json={
                    "consumer_group": group_id,
                    "client_agent": "e2e-test",
                    "interests": [
                        {"topic": TOPIC, "tenant_id": tenant_id, "types": [EVENT_TYPE]}
                    ],
                },
            )
            assert sub1_resp.status_code == 201
            sub1_id = sub1_resp.json()["id"]

            seek1 = await client.post(
                f"/subscriptions/{sub1_id}:seek",
                json={
                    "partition_positions": [
                        {"topic": TOPIC, "partition": 0, "value": "earliest"}
                    ]
                },
            )
            assert seek1.status_code == 200

        event_id = str(uuid.uuid4())
        async with httpx.AsyncClient(base_url=base_url, timeout=None) as stream_client:
            async with stream_client.stream(
                "GET", f"/events:sse?subscription_id={sub1_id}"
            ) as stream1_resp:
                assert stream1_resp.status_code == 200
                reader1 = SseFrameReader(stream1_resp)
                kind, _ = await reader1.next_frame(timeout=5)
                assert kind == "topology"

                async with httpx.AsyncClient(base_url=base_url, timeout=5.0) as pub_client:
                    publish_resp = await pub_client.post(
                        "/events",
                        json={
                            "id": event_id,
                            "type": EVENT_TYPE,
                            "tenant_id": tenant_id,
                            "source": "e2e-test",
                            "subject": "s1",
                            "subject_type": SUBJECT_TYPE,
                            "occurred_at": datetime.now(timezone.utc).isoformat(),
                        },
                    )
                assert publish_resp.status_code == 202

                # The Cursor row this test goes on to check across the
                # restart was written by the `:seek` above, not by consuming:
                # SEEK is the cursor's only writer. Reading the event here
                # proves the stream is live before the process is killed.
                kind, data = await reader1.next_frame(timeout=5)
                assert kind == "event"
                assert data["payload"]["id"] == event_id
                assert data["payload"]["sequence"] == 1
    finally:
        _stop(proc)

    # Restart against the SAME home_dir/SQLite file, on the same port.
    proc = _start(home_dir)
    try:
        async with httpx.AsyncClient(base_url=base_url, timeout=5.0) as client:
            # ConsumerGroupRepo is SQLite-backed - unchanged, must still exist.
            group_resp_after = await client.get(f"/consumer-groups/{group_id}")
            assert group_resp_after.status_code == 200
            assert group_resp_after.json() == group_body_before

            # SubscriptionRepo is ClusterCacheV1-backed - ephemeral under the
            # standalone cache provider, so it must NOT survive.
            sub1_resp_after = await client.get(f"/subscriptions/{sub1_id}")
            assert sub1_resp_after.status_code == 404

            # A brand-new subscription under the same consumer group,
            # deliberately not seeked: `stream()` only rejects an unseeded
            # partition (409 PositionsNotSet) when `find_cursor` finds
            # nothing. Getting 200 here is the proof the Cursor row from
            # before the restart survived.
            sub2_resp = await client.post(
                "/subscriptions",
                json={
                    "consumer_group": group_id,
                    "client_agent": "e2e-test",
                    "interests": [
                        {"topic": TOPIC, "tenant_id": tenant_id, "types": [EVENT_TYPE]}
                    ],
                },
            )
            assert sub2_resp.status_code == 201
            sub2_id = sub2_resp.json()["id"]

        async with httpx.AsyncClient(base_url=base_url, timeout=None) as stream_client:
            async with stream_client.stream(
                "GET", f"/events:sse?subscription_id={sub2_id}"
            ) as stream2_resp:
                assert stream2_resp.status_code == 200
                reader2 = SseFrameReader(stream2_resp)
                kind, _ = await reader2.next_frame(timeout=5)
                assert kind == "topology"

                # The event row itself - not just the cursor row - is still
                # in the SQLite EventBrokerBackend table: a fresh
                # subscription's in-memory replay cursor starts from its own
                # join-time `assigned.offset` (always 0), so it re-reads
                # from the start rather than skipping ahead.
                kind, data = await reader2.next_frame(timeout=5)
                assert kind == "event"
                assert data["payload"]["id"] == event_id
                assert data["payload"]["sequence"] == 1
    finally:
        _stop(proc)
