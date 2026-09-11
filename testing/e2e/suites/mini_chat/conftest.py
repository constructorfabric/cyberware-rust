# Updated: 2026-04-16 by Constructor Tech
"""Mini-chat E2E conftest — SSE helpers, provider fixtures, gear test env."""

from __future__ import annotations

import json
import os
import re
import sqlite3
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path

import pytest
import httpx

from .mock_provider.server import MockProviderServer, DummyMockProvider


# ── Constants ─────────────────────────────────────────────────────────────

BASE_URL = os.environ.get("BASE_URL", "http://127.0.0.1:8087")
API_PREFIX = f"{BASE_URL}/cf/mini-chat/v1"

# NOTE: model_id is unique across providers in this test catalog by convention,
# not by system design. The production model policy plugin could map the same
# model_id to different providers. Tests rely on this uniqueness for simplicity.
DEFAULT_MODEL = "azure-gpt-4.1"       # Premium tier, Azure (production target)
STANDARD_MODEL = "gpt-5.2"            # Standard tier, OpenAI

PROVIDER_DEFAULT_MODEL = {
    "azure": DEFAULT_MODEL,
    "openai": STANDARD_MODEL,
}

_TEMP_HOME = tempfile.mkdtemp(prefix="cf-gears-test-")
DB_PATH = os.path.join(_TEMP_HOME, "mini-chat", "mini_chat.db")

MODULE_DIR = Path(__file__).resolve().parent


# ── SSE helpers ───────────────────────────────────────────────────────────

@dataclass
class SSEEvent:
    """Parsed SSE event."""
    event: str
    data: dict | str = field(default_factory=dict)


def parse_sse(text: str) -> list[SSEEvent]:
    """Parse SSE text into a list of events."""
    events = []
    current_event = None
    current_data_lines: list[str] = []

    for line in text.split("\n"):
        if line.startswith("event:"):
            current_event = line[len("event:"):].strip()
            current_data_lines = []
        elif line.startswith("data:"):
            current_data_lines.append(line[len("data:"):].strip())
        elif line == "" and current_event is not None:
            raw = "\n".join(current_data_lines)
            try:
                data = json.loads(raw)
            except (json.JSONDecodeError, ValueError):
                data = raw
            events.append(SSEEvent(event=current_event, data=data))
            current_event = None
            current_data_lines = []

    if current_event is not None:
        raw = "\n".join(current_data_lines)
        try:
            data = json.loads(raw)
        except (json.JSONDecodeError, ValueError):
            data = raw
        events.append(SSEEvent(event=current_event, data=data))

    return events


def expect_stream_started(events: list[SSEEvent]) -> SSEEvent:
    """Find the 'stream_started' event or fail with diagnostics."""
    for e in events:
        if e.event == "stream_started":
            return e
    event_types = [e.event for e in events]
    raise AssertionError(f"No 'stream_started' event. Events received: {event_types}")


def expect_done(events: list[SSEEvent]) -> SSEEvent:
    """Find the 'done' event or fail with diagnostics."""
    for e in events:
        if e.event == "done":
            return e
    error_events = [e for e in events if e.event == "error"]
    event_types = [e.event for e in events]
    if error_events:
        raise AssertionError(
            f"Stream ended with error instead of done: {error_events[0].data}\n"
            f"Events received: {event_types}"
        )
    raise AssertionError(f"No 'done' event in stream. Events received: {event_types}")


def _log_request(method: str, url: str, body=None, status: int = 0, response_text: str = ""):
    import logging
    log = logging.getLogger("mini-chat-test")
    log.info(f">>> {method} {url}")
    if body:
        log.info(f">>> Body: {json.dumps(body, default=str)[:500]}")
    log.info(f"<<< {status}")
    if response_text:
        log.info(f"<<< {response_text[:500]}")


def poll_until(call, *, until, timeout: int = 60):
    """Generic polling helper. call() returns an httpx.Response, until(resp) returns bool."""
    import time
    deadline = time.monotonic() + timeout
    resp = None
    while time.monotonic() < deadline:
        resp = call()
        assert resp.status_code == 200, f"Poll failed: {resp.status_code} {resp.text}"
        if until(resp):
            return resp
        time.sleep(1)
    raise TimeoutError(
        f"Polling timed out after {timeout}s. Last response: {resp.text[:200] if resp else 'none'}"
    )


def stream_message(chat_id: str, content: str, **kwargs) -> tuple[int, list[SSEEvent], str]:
    """Send a streaming message and return (status_code, events, raw_body)."""
    body = {"content": content, **kwargs}
    url = f"{API_PREFIX}/chats/{chat_id}/messages:stream"
    resp = httpx.post(
        url, json=body,
        headers={"Accept": "text/event-stream"},
        timeout=90,
    )
    raw = resp.text
    _log_request("POST", url, body, resp.status_code, raw)
    events = parse_sse(raw) if resp.status_code == 200 else []
    return resp.status_code, events, raw


# ── Config patching ───────────────────────────────────────────────────────

_REQUIRED_ONLINE = ["OPENAI_API_KEY", "AZURE_OPENAI_API_KEY"]


def _patch_mini_chat_config(config_text: str, env) -> str:
    """Patch mini-chat config based on mode (offline/online)."""
    from .config.generator import load_credentials

    # home_dir
    config_text = re.sub(r"(home_dir\s*:\s*).*", rf'\1"{_TEMP_HOME}"', config_text, count=1)

    # Log level — mini_chat logging is already in base.yaml; only inject oagw
    mini_chat_log = os.environ.get("MINI_CHAT_LOG", "debug")
    log_inject = (
        f"  oagw:\n"
        f"    console_level: {mini_chat_log}\n"
    )
    config_text = config_text.replace("  api-gateway:", log_inject + "  api-gateway:", 1)

    # Find mock provider sidecar (if any)
    mock_port = None
    for sc in env.sidecars:
        if sc.name == "mock-provider" and sc.port is not None:
            mock_port = sc.port
            break

    if mock_port is not None:
        # Offline mode — patch hosts to mock
        mock_host = "127.0.0.1"
        config_text = config_text.replace('host: "api.openai.com"', f'host: "{mock_host}"', 1)
        match = re.search(r'(azure_openai:.*?host:\s*")([^"]+)(")', config_text, re.DOTALL)
        if match:
            config_text = config_text[:match.start(2)] + mock_host + config_text[match.end(2):]

        # Inject port/use_http/upstream_alias
        for marker, alias in [("openai:", "mock-openai"), ("azure_openai:", "mock-azure")]:
            m = re.search(rf"({marker}.*?)(host:\s*\"[^\"]+\")", config_text, re.DOTALL)
            if m:
                inject = (
                    f'\n          port: {mock_port}'
                    f'\n          use_http: true'
                    f'\n          upstream_alias: "{alias}"'
                )
                config_text = config_text[:m.end(2)] + inject + config_text[m.end(2):]

        # Enable HTTP upstream
        if "allow_http_upstream" in config_text:
            config_text = config_text.replace("allow_http_upstream: false", "allow_http_upstream: true")
        else:
            config_text = config_text.replace(
                "proxy_timeout_secs:", "allow_http_upstream: true\n      proxy_timeout_secs:",
            )

        # Dummy creds
        config_text = config_text.replace("REPLACE_WITH_OPENAI_KEY", "mock-key-openai")
        config_text = config_text.replace("REPLACE_WITH_AZURE_KEY", "mock-key-azure")
    else:
        # Online mode — real creds from env
        creds = load_credentials()
        config_text = config_text.replace("REPLACE_WITH_OPENAI_KEY", creds.get("OPENAI_API_KEY", ""))
        config_text = config_text.replace("REPLACE_WITH_AZURE_KEY", creds.get("AZURE_OPENAI_API_KEY", ""))
        azure_host = creds.get("AZURE_OPENAI_HOST")
        if azure_host:
            match = re.search(r'(azure_openai:.*?host:\s*")([^"]+)(")', config_text, re.DOTALL)
            if match:
                config_text = config_text[:match.start(2)] + azure_host + config_text[match.end(2):]

    return config_text


# ── DB summary ────────────────────────────────────────────────────────────

def _print_db_summary() -> str | None:
    _lines: list[str] = []

    def out(msg=""):
        _lines.append(msg)

    if not os.path.exists(DB_PATH):
        return f"!! DB summary skipped: {DB_PATH} does not exist"

    try:
        conn = sqlite3.connect(f"file:{DB_PATH}?mode=ro", uri=True)
        conn.row_factory = sqlite3.Row
    except Exception as exc:
        return f"!! DB summary skipped: cannot open DB: {exc}"

    sep = "=" * 110
    out(f"\n{sep}")
    out("  POST-RUN DB SUMMARY")
    out(f"  DB: {DB_PATH}")
    out(sep)

    try:
        rows = [dict(r) for r in conn.execute(
            "SELECT * FROM quota_usage ORDER BY period_type, bucket"
        ).fetchall()]
        if rows:
            out(f"\n  QUOTA_USAGE ({len(rows)} rows)")
            out(
                f"  {'period':<8} {'bucket':<16} {'spent_cr':>12} {'reserved_cr':>12} "
                f"{'calls':>6} {'in_tok':>8} {'out_tok':>9} {'ws_calls':>9} "
                f"{'fs_calls':>9} {'rag_calls':>10} {'img_in':>7} {'img_bytes':>10}"
            )
            out(f"  {'-' * 96}")
            for q in rows:
                out(
                    f"  {q['period_type']:<8} "
                    f"{q['bucket']:<16} "
                    f"{q['spent_credits_micro']:>12} "
                    f"{q['reserved_credits_micro']:>12} "
                    f"{q['calls']:>6} "
                    f"{q['input_tokens']:>8} "
                    f"{q['output_tokens']:>9} "
                    f"{q['web_search_calls']:>9} "
                    f"{q['file_search_calls']:>9} "
                    f"{q['rag_retrieval_calls']:>10} "
                    f"{q['image_inputs']:>7} "
                    f"{q['image_upload_bytes']:>10}"
                )
            stuck = [q for q in rows if q["reserved_credits_micro"] != 0]
            total_daily = [q for q in rows if q["bucket"] == "total" and q["period_type"] == "daily"]
            if stuck:
                out(f"\n  !! STUCK RESERVES: {len(stuck)} rows with reserved_credits_micro != 0")
            else:
                out("\n  OK No stuck reserves (all reserved_credits_micro = 0)")
            if total_daily:
                td = total_daily[0]
                out(f"  OK Daily totals: {td['calls']} calls, "
                    f"{td['input_tokens']} in_tok, {td['output_tokens']} out_tok, "
                    f"{td['web_search_calls']} ws_calls, "
                    f"{td['spent_credits_micro']} credits spent")
    except Exception as exc:
        out(f"\n  !! quota_usage query failed: {exc}")

    try:
        turns = [dict(r) for r in conn.execute(
            "SELECT t.state, t.effective_model, "
            "       t.reserve_tokens, t.max_output_tokens_applied, "
            "       t.reserved_credits_micro, t.error_code, "
            "       m.input_tokens AS actual_in, m.output_tokens AS actual_out "
            "FROM chat_turns t "
            "LEFT JOIN messages m ON m.id = t.assistant_message_id "
            "WHERE t.deleted_at IS NULL ORDER BY t.started_at"
        ).fetchall()]
        if turns:
            WS_THRESHOLD = 2000
            groups: dict[tuple, dict] = {}
            for t in turns:
                model = t["effective_model"] or "?"
                state = t["state"]
                ws = "ws" if (t["actual_in"] or 0) > WS_THRESHOLD else "plain"
                err = t["error_code"] or ""
                key = (model, state, ws, err)
                if key not in groups:
                    groups[key] = {
                        "count": 0, "reserve_tok": [], "reserved_cr": [],
                        "actual_in": [], "actual_out": [],
                    }
                g = groups[key]
                g["count"] += 1
                g["reserve_tok"].append(t["reserve_tokens"] or 0)
                g["reserved_cr"].append(t["reserved_credits_micro"] or 0)
                g["actual_in"].append(t["actual_in"] or 0)
                g["actual_out"].append(t["actual_out"] or 0)

            out(f"\n  CHAT_TURNS ({len(turns)} turns, {len(groups)} groups)")
            out(
                f"  {'model':<14} {'state':<10} {'type':<6} {'cnt':>4} "
                f"{'reserve_tok':>11} {'reserved_cr':>12} "
                f"{'avg_in':>8} {'avg_out':>9} {'error':>16}"
            )
            out(f"  {'-' * 96}")
            for (model, state, ws, err), g in groups.items():
                n = g["count"]
                avg_in = sum(g["actual_in"]) // n
                avg_out = sum(g["actual_out"]) // n
                avg_res_tok = sum(g["reserve_tok"]) // n
                avg_res_cr = sum(g["reserved_cr"]) // n
                out(
                    f"  {model:<14} {state:<10} {ws:<6} {n:>4} "
                    f"{avg_res_tok:>11} {avg_res_cr:>12} "
                    f"{avg_in:>8} {avg_out:>9} {err:>16}"
                )

            states: dict[str, int] = {}
            for t in turns:
                states[t["state"]] = states.get(t["state"], 0) + 1
            out(f"\n  Total: {', '.join(f'{v} {k}' for k, v in states.items())}")
    except Exception as exc:
        out(f"\n  !! chat_turns query failed: {exc}")

    try:
        msg_stats = conn.execute(
            "SELECT role, COUNT(*) as cnt, "
            "       SUM(input_tokens) as total_in, SUM(output_tokens) as total_out "
            "FROM messages WHERE deleted_at IS NULL GROUP BY role"
        ).fetchall()
        if msg_stats:
            out("\n  MESSAGES")
            for m in msg_stats:
                m = dict(m)
                out(f"  {m['role']:<12} {m['cnt']} messages, "
                    f"in_tok={m['total_in']}, out_tok={m['total_out']}")
    except Exception as exc:
        out(f"\n  !! messages query failed: {exc}")

    conn.close()
    out(f"\n{sep}\n")
    return "\n".join(_lines)


# ── pytest hooks ──────────────────────────────────────────────────────────

def pytest_configure(config):
    """Register mini-chat markers."""
    config.addinivalue_line("markers", "openai: Tests targeting OpenAI provider")
    config.addinivalue_line("markers", "azure: Tests targeting Azure OpenAI provider")
    config.addinivalue_line("markers", "multi_provider: Tests requiring multiple providers")
    config.addinivalue_line("markers", "online_only: Tests that require real cloud (skipped in offline mode)")


def pytest_addoption(parser):
    """Register mini-chat E2E mode flag."""
    parser.addoption(
        "--mode",
        choices=["offline", "online"],
        default="offline",
        help="offline = mock LLM provider (default, no keys); online = real cloud providers",
    )


def pytest_collection_modifyitems(config, items):
    """Auto-skip online_only tests in offline mode."""
    if config.getoption("mode") == "offline":
        skip = pytest.mark.skip(reason="requires --mode online")
        for item in items:
            if "online_only" in item.keywords:
                item.add_marker(skip)


@pytest.fixture(scope="session", autouse=True)
def _check_mini_chat_binary():
    """Skip all mini-chat tests when E2E_BINARY is not set.

    In CI, make e2e-local runs all gears against the shared server.
    Mini-chat needs its own binary (different features), built separately
    via make e2e-mini-chat. Without E2E_BINARY, skip gracefully.
    """
    if not os.environ.get("E2E_BINARY"):
        pytest.skip(
            "E2E_BINARY not set — run mini-chat tests via: make e2e-mini-chat",
            allow_module_level=True,
        )


_db_summary_text: str | None = None


def pytest_terminal_summary(terminalreporter, exitstatus, config):
    if _db_summary_text:
        terminalreporter.write_line("")
        for line in _db_summary_text.split("\n"):
            terminalreporter.write_line(line)


# ── GearTestEnv (orchestrator integration) ──────────────────────────────

@pytest.fixture(scope="session")
def gear_test_env(request):
    """mini-chat gear test environment."""
    from lib.orchestrator import GearTestEnv

    mode = request.config.getoption("mode")
    mock = MockProviderServer() if mode == "offline" else DummyMockProvider()

    mini_chat_log = os.environ.get("MINI_CHAT_LOG", "debug")
    rust_log = os.environ.get(
        "RUST_LOG", f"info,mini_chat={mini_chat_log},oagw={mini_chat_log}",
    )

    return GearTestEnv(
        # Binary resolved from E2E_BINARY env var, or found in PATH/target.
        config_path=MODULE_DIR / "config" / "base.yaml",
        config_patch=_patch_mini_chat_config,
        port=8087,
        health_path="/cf/openapi.json",
        health_timeout=90,
        env={"RUST_LOG": rust_log},
        sidecars=[mock],
        log_suffix="mini-chat",
    )


# ── Fixtures ──────────────────────────────────────────────────────────────

@pytest.fixture(scope="session")
def server(test_env):
    """Alias for backward compat — yields base URL after server is running."""
    global _db_summary_text
    yield test_env.base_url
    # DB summary on teardown
    import time
    time.sleep(3)
    _db_summary_text = _print_db_summary()


# Credential type for a plain opaque secret (ADR-0004).
_GENERIC_TYPE = "gts.cf.core.credstore.credential.v1~cf.core.credstore.generic.v1~"


@pytest.fixture(scope="session", autouse=True)
def _provision_credstore_secrets(request, server):
    """Provision the LLM provider secrets through the credstore gateway.

    mini-chat routes LLM calls through OAGW, whose auth plugin resolves the
    provider ``secret_ref`` (``openai-key`` / ``azure-openai-key``) via the
    now-stateful credstore gateway. The gateway resolves a secret's metadata
    from its own DB before reading the value, so the secrets must be created
    through its API — pre-seeding the static plugin config alone is no longer
    reachable; the static plugin's config seeding is gone entirely (ADR-0006).

    The mini-chat rig is single-tenant (``single-tenant-tr-plugin``) with
    ``accept_all`` authn, so a ``tenant``-scoped secret resolves for the S2S
    context OAGW uses when proxying. Offline mode (default) uses mock values
    (the mock provider ignores them); online mode uses the real env keys, the
    same ones ``_patch_mini_chat_config`` would otherwise inject.

    Create-or-replace: creation is a single ``PUT`` carrying both the record
    and the value (ADR-0004) — there is no POST. We PUT with
    ``If-None-Match: *`` (create-only), and on 409 — a rerun against an
    already-provisioned rig — PUT again with the explicit ``If-Match: *``
    overwrite (a replace never needs ``type``). Reruns stay safe.
    """
    if request.config.getoption("mode") == "online":
        secrets = {
            "openai-key": os.environ.get("OPENAI_API_KEY", ""),
            "azure-openai-key": os.environ.get("AZURE_OPENAI_API_KEY", ""),
        }
    else:
        secrets = {
            "openai-key": "mock-key-openai",
            "azure-openai-key": "mock-key-azure",
        }
    headers = {
        "Authorization": "Bearer mini-chat-e2e",
        "content-type": "application/json",
    }
    # Iterate over a plain tuple of reference names (not `secrets.items()`) so
    # nothing derived from the secret values flows into log/error messages,
    # and never echo the response body — it could reflect the secret.
    provisioned = 0
    reachable = True
    for ref in ("openai-key", "azure-openai-key"):
        value = secrets[ref]
        if not value:
            continue  # online mode without that provider's key configured
        try:
            # The mini-chat rig serves all routes under the api-gateway
            # `prefix_path: "/cf"` (same prefix as API_PREFIX above).
            resp = httpx.put(
                f"{server}/cf/credstore/v1/credentials/{ref}",
                headers={**headers, "If-None-Match": "*"},
                json={"type": _GENERIC_TYPE, "sharing": "tenant", "value": value},
                timeout=5.0,
            )
            if resp.status_code == 409:
                # Already provisioned (rerun): overwrite in place.
                resp = httpx.put(
                    f"{server}/cf/credstore/v1/credentials/{ref}",
                    headers={**headers, "If-Match": "*"},
                    json={"sharing": "tenant", "value": value},
                    timeout=5.0,
                )
        except httpx.RequestError:
            # Any transport-level error (connect/timeout/read-timeout) — keep
            # provisioning non-fatal and fall through to the `yield`. `break`,
            # not `return`: a bare return before the `yield` in this generator
            # fixture raises "did not yield a value" and ERRORs the whole
            # session (review finding #5). Broadened from `ConnectError` so a
            # read-timeout against a still-warming server is caught too.
            reachable = False
            break
        if resp.status_code not in (201, 204):
            msg = f"could not provision credstore secret {ref!r}: HTTP {resp.status_code}"
            if request.config.getoption("mode") == "offline":
                # Offline mode is deterministic — a provisioning failure is a real
                # problem, so fail fast instead of letting tests fail opaquely.
                raise RuntimeError(f"[e2e] {msg}")
            print(f"[e2e] WARN: {msg}")
        else:
            provisioned += 1

    # mini-chat registers its OAGW provider upstreams at boot, but the stateful
    # credstore has no secret then, so registration is deferred and retried in
    # the background once the secret exists (which is exactly what we just did).
    # Wait for that reconcile to converge so provider-dependent tests don't race
    # it and see a transient "Upstream not found". Skip the wait when the rig
    # was unreachable — _await_oagw_upstreams would only time out.
    if reachable:
        _await_oagw_upstreams(server, headers, provisioned)
    yield


def _await_oagw_upstreams(server, headers, expected, timeout=60.0):
    """Best-effort wait until at least ``expected`` OAGW upstreams are live.

    The mini-chat gear retries deferred provider registration on a short cadence
    after its secret is provisioned; poll ``GET /oagw/v1/upstreams`` (a JSON
    array) until the provider upstreams appear, then let routes settle. This
    only *waits* — it never fails the session; the tests' own assertions remain
    the source of truth.

    ``expected`` is the number of secrets provisioned, used as a proxy for the
    number of provider upstreams. That holds for this rig (each provider has one
    distinct ``secret_ref`` and no ``tenant_overrides``); if the config gains
    tenant overrides or shares a ``secret_ref`` across providers, revisit this
    count (upstream count would no longer equal secret count).

    Robustness: a persistently non-observable endpoint (e.g. an authz change
    returning non-200) is detected within a short probe window and we proceed
    rather than spinning the full timeout; a non-JSON/invalid body is treated
    as "not ready yet" rather than raising.
    """
    if expected == 0:
        return
    start = time.monotonic()
    deadline = start + timeout
    probe_window = min(10.0, timeout)  # time allowed to observe *any* 200
    saw_endpoint = False
    while time.monotonic() < deadline:
        count = None
        try:
            resp = httpx.get(f"{server}/cf/oagw/v1/upstreams", headers=headers, timeout=5.0)
            if resp.status_code == 200:
                saw_endpoint = True
                body = resp.json()
                if isinstance(body, list):
                    count = len(body)
        except (httpx.RequestError, ValueError):
            pass  # transport error or non-JSON body — treat as "not ready yet"

        if count is not None and count >= expected:
            # Upstreams are up; give per-provider route registration (which runs
            # just after each create_upstream) a brief moment to catch up.
            time.sleep(1.0)
            return
        # If the endpoint never became observable within the probe window, it is
        # likely gated (authz/route); stop waiting and let the tests decide.
        if not saw_endpoint and time.monotonic() - start >= probe_window:
            print("[e2e] WARN: OAGW upstreams endpoint not observable; proceeding")
            return
        time.sleep(0.5)
    print(f"[e2e] WARN: OAGW upstreams did not reach >= {expected} within {timeout:.0f}s")


@pytest.fixture(scope="session")
def mock_provider(test_env):
    """Access to the mock provider sidecar (for set_next_scenario)."""
    return test_env.sidecars.get("mock-provider")


@pytest.fixture(autouse=True)
def reset_mock_provider_state(mock_provider):
    """Prevent session-scoped mock sidecar state from leaking between tests."""
    mock_provider.clear_captured_requests()
    mock_provider.clear_override_scenarios()
    mock_provider.clear_state()
    yield
    mock_provider.clear_captured_requests()
    mock_provider.clear_override_scenarios()
    mock_provider.clear_state()


@pytest.fixture
def chat(server) -> dict:
    """Create a fresh chat with the default model (azure-gpt-4.1)."""
    resp = httpx.post(f"{API_PREFIX}/chats", json={})
    assert resp.status_code == 201, f"Failed to create chat: {resp.status_code} {resp.text}"
    return resp.json()


@pytest.fixture
def chat_with_model(server):
    """Factory fixture: create a chat with a specific model."""
    def _create(model: str) -> dict:
        resp = httpx.post(f"{API_PREFIX}/chats", json={"model": model})
        assert resp.status_code == 201, f"Failed to create chat: {resp.status_code} {resp.text}"
        return resp.json()
    return _create


# ── Provider-parameterized fixtures ───────────────────────────────────────

@pytest.fixture(params=[
    pytest.param("openai", marks=pytest.mark.openai),
    pytest.param("azure", marks=pytest.mark.azure),
])
def provider(request):
    """Current provider under test — parameterized, auto-marked."""
    return request.param


@pytest.fixture
def provider_chat(provider, chat_with_model) -> dict:
    """Chat using the default model for the current provider."""
    return chat_with_model(PROVIDER_DEFAULT_MODEL[provider])
