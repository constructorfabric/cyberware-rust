"""E2E fixtures for the usage-collector gear.

This module owns its own server AND its own database container, because the
gear's storage plugin connects and migrates a real database at init and fails
hard when it cannot. It therefore gates on E2E_BINARY and skips when unset,
exactly like mini_chat: `make e2e-local` builds a binary WITHOUT the
usage-collector features, so running there would start a second server and a
container for routes that binary does not serve.

The suite runs against either storage backend, selected by `UC_E2E_BACKEND`
(see BACKENDS below). Only this module knows which one is active: the test
bodies talk HTTP and are backend-agnostic. Because a plugin compiled into the
binary always initializes — and both plugins fail init without their own
database — each backend needs its own binary, built from its own cargo feature
set, and the two run sequentially rather than in one process.

Run it with: make e2e-usage-collector
"""

from __future__ import annotations

import os
import uuid
from datetime import datetime, timedelta, timezone
from pathlib import Path

import httpx
import pytest

from lib.orchestrator import GearTestEnv
from lib.sidecars import ClickHouseSidecar, TimescaleDbSidecar, skip_without_docker

# ── Constants ─────────────────────────────────────────────────────────────

HERE = Path(__file__).resolve().parent
PROJECT_ROOT = Path(__file__).resolve().parents[4]

# Per-backend wiring: config file, sidecar class, the DSN port placeholder that
# config carries, and the plugin crate to raise to debug logging.
BACKENDS = {
    "timescaledb": {
        "config": "config.yaml",
        "sidecar": TimescaleDbSidecar,
        "placeholder": "__E2E_TS_PORT__",
        "log_crate": "cf_gears_timescaledb_usage_collector_plugin",
    },
    "clickhouse": {
        "config": "config-clickhouse.yaml",
        "sidecar": ClickHouseSidecar,
        "placeholder": "__E2E_CH_PORT__",
        "log_crate": "cf_gears_clickhouse_usage_collector_plugin",
    },
}

# Defaults to timescaledb so a bare `pytest testing/e2e/suites/usage_collector/`
# behaves as it always has; the Makefile sets this explicitly for each run.
BACKEND = os.environ.get("UC_E2E_BACKEND", "timescaledb")
if BACKEND not in BACKENDS:
    raise RuntimeError(
        f"UC_E2E_BACKEND={BACKEND!r} is not one of {sorted(BACKENDS)}"
    )

_BACKEND_CFG = BACKENDS[BACKEND]
CONFIG = HERE / _BACKEND_CFG["config"]
SIDECAR = _BACKEND_CFG["sidecar"]
PORT_PLACEHOLDER = _BACKEND_CFG["placeholder"]

SERVER_PORT = 8088
API_BASE = "/usage-collector/v1"
REQUEST_TIMEOUT = 5.0

# Must match both backend configs; they share one tenant/token set on purpose.
TENANT_A = "a0000000-0000-4000-8000-00000000000a"
TENANT_B = "a0000000-0000-4000-8000-00000000000b"
TOKEN_A = "e2e-uc-token-tenant-a"
TOKEN_B = "e2e-uc-token-tenant-b"

# Reserved abstract base every usage type derives from
# (usage_collector_sdk::UsageTypeGtsId::USAGE_RECORD_BASE).
GTS_BASE = "gts.cf.core.uc.usage_record.v1~"


def unique_suffix() -> str:
    """Short lowercase-alphanumeric token, legal inside a GTS segment."""
    return uuid.uuid4().hex[:8]


def usage_type_gts_id(suffix: str) -> str:
    """Build a derived usage-type GTS instance id.

    Segment grammar is vendor.package.namespace.type.v<major>; `_` is a legal
    namespace.
    """
    return f"{GTS_BASE}cf.uc_e2e._.usage_{suffix}.v1"


def time_window(minutes: int = 60) -> tuple[str, str]:
    """An RFC3339 (from, to) pair straddling `now`.

    Both GET /records and POST /records/aggregate REQUIRE a bounded created_at
    window as top-level `and` conjuncts; without one they return 400
    MISSING_TIME_WINDOW.
    """
    now = datetime.now(timezone.utc)
    frm = (now - timedelta(minutes=minutes)).strftime("%Y-%m-%dT%H:%M:%SZ")
    to = (now + timedelta(minutes=minutes)).strftime("%Y-%m-%dT%H:%M:%SZ")
    return frm, to


def window_filter(minutes: int = 60) -> str:
    frm, to = time_window(minutes)
    return f"created_at ge {frm} and created_at lt {to}"


def now_rfc3339() -> str:
    """Current UTC instant, whole seconds.

    Always `now` — never a fixed past instant — so retention can never
    interact and no test asserts an absolute date.
    """
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


# ── Environment gate ──────────────────────────────────────────────────────

@pytest.fixture(scope="session", autouse=True)
def _require_dedicated_binary():
    if not os.environ.get("E2E_BINARY"):
        pytest.skip(
            "E2E_BINARY not set — run these tests via: make e2e-usage-collector",
            allow_module_level=True,
        )
    skip_without_docker()


def pytest_collection_modifyitems(items):
    """Stop charging harness startup to the per-test budget in THIS package.

    `pytest_collection_modifyitems` is a global hook: pytest invokes every
    registered conftest's implementation once with the FULL collected item
    list — including tests from every other gear suite under `testing/e2e`.
    This conftest must filter to items under `HERE` (this directory) before
    touching them, or it would change pytest-timeout's behaviour for unrelated
    suites too.

    The problem: pytest-timeout charges FIXTURE SETUP against whichever test
    triggers it. The session-scoped `test_env` starts a database container
    (image pull included) and waits up to 90s for the server to become
    healthy, so the first test to run would blow pytest.ini's 10s budget
    through no fault of its own.

    The fix is `func_only`, NOT a bigger number. `func_only=True` moves
    pytest-timeout's timer from the whole runtest protocol to the call phase
    alone, and passing no timeout value leaves pytest.ini's 10s in force
    (pytest_timeout._get_item_settings falls back to the ini value whenever
    the marker omits one). So every test here keeps the repo-wide 10s hard
    kill on its own body — `docs/toolkit_unified_system/13_e2e_testing.md`
    §3: "If a test exceeds 10s, it is broken, not slow" — while the harness's
    startup cost is simply not counted. Raising the number instead would let
    a genuinely hung test burn the raised budget.

    Session setup keeps its own bounds and its own diagnostics: READY_TIMEOUT
    (60s) and DOCKER_TIMEOUT (120s) in `lib/sidecars.py`, `health_timeout`
    (90s) in the orchestrator. Those report *what* stalled; a pytest-timeout
    kill mid-setup only dumps stacks.

    An explicit @pytest.mark.timeout on a test wins — that is how the
    container lifecycle tests keep their own longer budget, and those do their
    waiting inside the test body where the timer belongs.

    Both sides of the path comparison are `.resolve()`d. pytest canonicalises
    neither `item.fspath` nor a conftest's `__file__`, so comparing them raw
    also works today — but resolving only ONE side silently matches nothing
    when the checkout path contains a symlink, which would drop the marker and
    hand the first test a 10s budget it cannot meet. Symmetry is the invariant
    here; keep both `.resolve()` calls or neither.
    """
    for item in items:
        if HERE not in Path(str(item.fspath)).resolve().parents:
            continue
        if item.get_closest_marker("timeout") is None:
            item.add_marker(pytest.mark.timeout(func_only=True))


# ── Test environment ──────────────────────────────────────────────────────

def _patch_config(config_text: str, env) -> str:
    """Substitute the sidecar's mapped port into the plugin DSN.

    The orchestrator calls this AFTER sidecars start, so the port is known.
    """
    for sidecar in env.sidecars:
        if sidecar.name == SIDECAR.name:
            return config_text.replace(PORT_PLACEHOLDER, sidecar.dsn_port)
    raise RuntimeError(
        f"{SIDECAR.name} sidecar missing from GearTestEnv.sidecars "
        f"(UC_E2E_BACKEND={BACKEND})"
    )


@pytest.fixture(scope="session")
def gear_test_env() -> GearTestEnv:
    return GearTestEnv(
        config_path=CONFIG,
        config_patch=_patch_config,
        port=SERVER_PORT,
        health_path="/healthz",
        health_timeout=90,
        env={"RUST_LOG": os.environ.get(
            "RUST_LOG",
            "info,cf_gears_usage_collector=debug,"
            f"{_BACKEND_CFG['log_crate']}=debug",
        )},
        sidecars=[SIDECAR()],
        # Backend-suffixed so the two runs do not overwrite each other's log.
        log_suffix=f"usage-collector-{BACKEND}",
    )


# ── HTTP helpers ──────────────────────────────────────────────────────────

@pytest.fixture
def api(test_env):
    """Async client factory bound to the running server, per tenant token."""
    def _client(token: str = TOKEN_A) -> httpx.AsyncClient:
        return httpx.AsyncClient(
            base_url=f"{test_env.base_url}{API_BASE}",
            headers={"Authorization": f"Bearer {token}"},
            timeout=REQUEST_TIMEOUT,
        )
    return _client


@pytest.fixture
def make_usage_type(api):
    """Register a usage type and return (gts_id, metadata_key).

    Setup only — no test asserts on the POST itself except the catalog tests.
    When the types-registry redesign lands and drops UC's usage-type REST
    surface, THIS FUNCTION is the single place that changes.
    """
    async def _create(kind: str = "counter", metadata_key: str = "region") -> tuple[str, str]:
        gts_id = usage_type_gts_id(unique_suffix())
        async with api() as client:
            resp = await client.post("/usage-types", json={
                "gts_id": gts_id,
                "kind": kind,
                "metadata_fields": [metadata_key],
            })
        assert resp.status_code == 201, f"usage-type setup failed: {resp.status_code} {resp.text}"
        return gts_id, metadata_key
    return _create


def record_payload(
    gts_id: str,
    *,
    tenant_id: str = TENANT_A,
    value: str = "1",
    resource_id: str = "res-1",
    idempotency_key: str | None = None,
    created_at: str | None = None,
    metadata: dict[str, str] | None = None,
) -> dict:
    """One CreateUsageRecordRequest. `value` is a STRING on the wire."""
    return {
        "gts_id": gts_id,
        "tenant_id": tenant_id,
        "resource_ref": {"resource_id": resource_id, "resource_type": "compute.vm"},
        "value": value,
        "idempotency_key": idempotency_key or f"e2e-idem-{unique_suffix()}",
        "created_at": created_at or now_rfc3339(),
        "metadata": metadata or {},
    }


def accepted_records(response: httpx.Response) -> list[dict]:
    """Assert every per-record outcome is `accepted`, return the record bodies.

    POST /records is a BATCH endpoint: it answers 200 when all records were
    accepted or deduplicated and 207 when any was rejected, with the real
    outcome per record. Asserting only on the status code would pass while
    every record was rejected.
    """
    assert response.status_code == 200, f"expected 200, got {response.status_code}: {response.text}"
    results = response.json()["results"]
    rejected = [r for r in results if r["outcome"] != "accepted"]
    assert not rejected, f"records rejected: {rejected}"
    return [r["record"] for r in results]
