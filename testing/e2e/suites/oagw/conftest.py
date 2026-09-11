"""Pytest configuration and fixtures for OAGW E2E tests."""
import asyncio
import os
import threading

import httpx
import pytest

from .mock_upstream import MockUpstreamServer


# ---------------------------------------------------------------------------
# Environment-driven fixtures
# ---------------------------------------------------------------------------

@pytest.fixture
def oagw_base_url():
    """OAGW service base URL."""
    return os.getenv("E2E_OAGW_BASE_URL", "http://localhost:8086")


@pytest.fixture
def mock_upstream_url():
    """Mock upstream base URL (must be reachable by the OAGW service)."""
    return os.getenv("E2E_MOCK_UPSTREAM_URL", "http://127.0.0.1:19876")


@pytest.fixture
def oagw_headers():
    """Standard headers for OAGW requests (auth only — tenant comes from the token)."""
    token = os.getenv("E2E_AUTH_TOKEN", "e2e-token-tenant-a")
    return {
        "Authorization": f"Bearer {token}",
    }


# ---------------------------------------------------------------------------
# Hierarchy tenant headers (for multi-tenant budget allocation tests)
# ---------------------------------------------------------------------------

@pytest.fixture
def hierarchy_root_headers():
    """Headers for hierarchy-root tenant (00000000-...001)."""
    return {"Authorization": "Bearer e2e-token-hierarchy-root"}


@pytest.fixture
def hierarchy_l1a_headers():
    """Headers for hierarchy-l1a tenant (00000000-...002), child of root."""
    return {"Authorization": "Bearer e2e-token-hierarchy-l1a"}


@pytest.fixture
def hierarchy_l1b_headers():
    """Headers for hierarchy-l1b tenant (00000000-...005), child of root."""
    return {"Authorization": "Bearer e2e-token-hierarchy-l1b"}


# ---------------------------------------------------------------------------
# Session-scoped mock upstream server
# ---------------------------------------------------------------------------

@pytest.fixture(scope="session")
def mock_upstream():
    """Start the mock upstream server for the entire test session."""
    url = os.getenv("E2E_MOCK_UPSTREAM_URL", "http://127.0.0.1:19876")

    # If a custom URL is set, assume the mock is managed externally.
    if os.getenv("E2E_MOCK_UPSTREAM_EXTERNAL"):
        yield
        return

    # Parse port from URL.
    port = int(url.rsplit(":", 1)[-1].split("/")[0])
    server = MockUpstreamServer(host="127.0.0.1", port=port)

    # Run the mock server in a background thread with its own event loop
    # so it can actually serve requests while tests run.
    loop = asyncio.new_event_loop()
    loop.run_until_complete(server.start())

    thread = threading.Thread(target=loop.run_forever, daemon=True)
    thread.start()

    yield server

    async def _shutdown() -> None:
        if server._server:
            server._server.close()
        current = asyncio.current_task()
        pending = [
            t for t in asyncio.all_tasks()
            if t is not current and not t.done()
        ]
        for task in pending:
            task.cancel()
        if pending:
            await asyncio.wait(pending, timeout=2)

    fut = asyncio.run_coroutine_threadsafe(_shutdown(), loop)
    try:
        fut.result(timeout=5)
    except (TimeoutError, Exception):
        pass  # Best-effort; the daemon thread will die with the process.
    loop.call_soon_threadsafe(loop.stop)
    thread.join(timeout=5)


# ---------------------------------------------------------------------------
# Session-scoped OAGW reachability check
# ---------------------------------------------------------------------------

@pytest.fixture(scope="session", autouse=True)
def _check_oagw_reachable():
    """Skip all OAGW tests if the service is not reachable."""
    url = os.getenv("E2E_OAGW_BASE_URL", "http://localhost:8086")
    try:
        resp = httpx.get(f"{url}/oagw/v1/upstreams", timeout=5.0)
        # Any response (even 401/403) means the service is up.
    except httpx.ConnectError:
        pytest.skip(f"OAGW service not running at {url}", allow_module_level=True)
    except Exception:
        # Timeout or other transient error — still try to run tests.
        pass


# ---------------------------------------------------------------------------
# Provision the secrets the auth-injection tests resolve via `cred://`
# ---------------------------------------------------------------------------

# Secrets the apikey / oauth2 auth plugins resolve via `cred://<ref>`.
# Values mirror the historical static-credstore-plugin seed in
# config/e2e-local.yaml.
_CREDSTORE_SECRETS = {
    "openai-key": "sk-test-e2e-fake-key",
    "test-oauth2-client-id": "test-client-id",
    "test-oauth2-client-secret": "test-client-secret",
}

# Credential type for a plain opaque secret (ADR-0004).
_GENERIC_TYPE = "gts.cf.core.credstore.credential.v1~cf.core.credstore.generic.v1~"


@pytest.fixture(scope="session", autouse=True)
def _provision_credstore_secrets(_check_oagw_reachable):
    """Write the auth-injection secrets through the credstore gateway API.

    The credstore gateway is now stateful: a GET resolves the secret's
    metadata from the gateway's own database first, then reads the value from
    the backend plugin. A secret merely pre-seeded in the static plugin config
    has no gateway metadata row and is therefore unreachable — the static
    plugin's config seeding is gone entirely (ADR-0006). So we create the
    secrets via the gateway API (which writes the metadata row *and* the
    backend value).

    Creation is a single ``PUT`` carrying both the record and the value
    (ADR-0004) — there is no POST. We PUT with ``If-None-Match: *``
    (create-only) and on 409 — a rerun against an already-provisioned rig —
    PUT again with the explicit ``If-Match: *`` overwrite (a replace never
    needs ``type``), with the same token the proxied requests carry
    (``E2E_AUTH_TOKEN`` -> tenant ``00000000-df51-...``). Sharing is
    ``tenant`` so any subject in that tenant resolves the value — this matches
    the proxied-request context regardless of subject_id (the historical seed
    used owner-bound ``private``; the gateway lookup does not depend on the
    OAGW auth-config ``sharing`` label).
    """
    base_url = os.getenv("E2E_OAGW_BASE_URL", "http://localhost:8086")
    token = os.getenv("E2E_AUTH_TOKEN", "e2e-token-tenant-a")
    headers = {
        "Authorization": f"Bearer {token}",
        "content-type": "application/json",
    }
    # Iterate over a literal tuple of reference names (not the value mapping)
    # so nothing derived from the secret values flows into log messages.
    for ref in ("openai-key", "test-oauth2-client-id", "test-oauth2-client-secret"):
        try:
            resp = httpx.put(
                f"{base_url}/credstore/v1/credentials/{ref}",
                headers={**headers, "If-None-Match": "*"},
                json={
                    "type": _GENERIC_TYPE,
                    "sharing": "tenant",
                    "value": _CREDSTORE_SECRETS[ref],
                },
                timeout=5.0,
            )
            if resp.status_code == 409:
                # Already provisioned (rerun): overwrite in place.
                resp = httpx.put(
                    f"{base_url}/credstore/v1/credentials/{ref}",
                    headers={**headers, "If-Match": "*"},
                    json={"sharing": "tenant", "value": _CREDSTORE_SECRETS[ref]},
                    timeout=5.0,
                )
        except httpx.RequestError:
            # Any transport-level error (connect/timeout/transport) — keep
            # provisioning non-fatal; the per-test reachability check skips tests.
            # `break`, not `return`: this is a generator fixture, so a bare
            # return before the `yield` below raises "did not yield a value"
            # and ERRORs the whole session instead of letting the per-test skip
            # take over (review finding #5).
            break
        if resp.status_code not in (201, 204):
            # Don't fail the session; the auth tests skip if the secret is
            # absent. Log only the ref name and status — never the response
            # body, which could reflect the secret value.
            print(
                f"[e2e] WARN: could not provision credstore secret {ref!r}: "
                f"HTTP {resp.status_code}"
            )
    yield
