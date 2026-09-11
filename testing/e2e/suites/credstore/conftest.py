"""Pytest fixtures for CredStore E2E tests (ADR-0004/ADR-0005: the
credential surface — ``/credstore/v1/credentials``).

The suite runs against the standard e2e server (``config/e2e-local.yaml``).
Tokens map to static identities (static-authn-plugin) inside the static
tenant tree (static-tr-plugin)::

    e2e-root (00000000-df51-...953)          <- e2e-token-tenant-a
      hierarchy-root (...0001)               <- e2e-token-hierarchy-root
        hierarchy-l1a (...0002)              <- e2e-token-hierarchy-l1a
        hierarchy-l1b (...0005)              <- e2e-token-hierarchy-l1b

``e2e-root`` is the root of the whole tree — an *ancestor* of
``hierarchy-root``, not a sibling of ``hierarchy-l1a``. Resolution walks
upward only, so a credential ``hierarchy-root`` shares is inherited by its
descendants (``hierarchy-l1a``/``hierarchy-l1b``) but is invisible to
``e2e-root`` itself, which never looks down its own subtree.

Every test creates its own uniquely-named credential (``unique_ref``) via
the ``create_credential`` factory and relies on ``cleanup`` (registered by
that factory) for best-effort teardown, so tests are order-independent and
re-runnable against a shared long-lived server.
"""
from __future__ import annotations

import os
import uuid

import httpx
import pytest

REQUEST_TIMEOUT = 5.0  # per-request hard timeout for all E2E calls

TENANT_A = "00000000-df51-5b42-9538-d2b56b7ee953"
HIERARCHY_ROOT = "00000000-0000-0000-0000-000000000001"
HIERARCHY_L1A = "00000000-0000-0000-0000-000000000002"
HIERARCHY_L1B = "00000000-0000-0000-0000-000000000005"


@pytest.fixture
def base_url():
    """API Gateway base URL."""
    return os.getenv("E2E_BASE_URL", "http://localhost:8086")


def _bearer(token: str) -> dict:
    return {"Authorization": f"Bearer {token}"}


@pytest.fixture
def tenant_a_headers():
    """Headers for e2e-root — the root of the whole tree, an ancestor of
    hierarchy-root (not a sibling of hierarchy-l1a/l1b)."""
    return _bearer(os.getenv("E2E_AUTH_TOKEN", "e2e-token-tenant-a"))


@pytest.fixture
def root_headers():
    """Headers for hierarchy-root (...0001) — parent of l1a and l1b."""
    return _bearer("e2e-token-hierarchy-root")


@pytest.fixture
def l1a_headers():
    """Headers for hierarchy-l1a (...0002), child of hierarchy-root."""
    return _bearer("e2e-token-hierarchy-l1a")


@pytest.fixture
def l1b_headers():
    """Headers for hierarchy-l1b (...0005), sibling of l1a."""
    return _bearer("e2e-token-hierarchy-l1b")


@pytest.fixture
def unique_ref():
    """Factory for unique credential references (safe on a shared server)."""

    def make(prefix: str = "e2e-cs") -> str:
        return f"{prefix}-{uuid.uuid4().hex[:12]}"

    return make


@pytest.fixture
def credentials_url(base_url):
    """CredStore credentials collection URL (ADR-0004)."""
    return f"{base_url}/credstore/v1/credentials"


@pytest.fixture
def cleanup(credentials_url):
    """Register ``(headers, ref)`` pairs; teardown deletes them best-effort
    with ``If-Match: *``. Populated by ``create_credential`` below, but
    tests may also register extra references directly (e.g. one created
    under a different tenant's headers)."""
    registered: list[tuple[dict, str]] = []

    def register(headers: dict, ref: str) -> str:
        registered.append((headers, ref))
        return ref

    yield register

    with httpx.Client(timeout=10.0) as client:
        for headers, ref in registered:
            try:
                client.delete(
                    f"{credentials_url}/{ref}",
                    headers={**headers, "If-Match": "*"},
                )
            except httpx.RequestError:
                continue


@pytest.fixture
def create_credential(credentials_url, cleanup):
    """Factory: create a credential via ``PUT .../{ref}`` with
    ``If-None-Match: *`` (create-only, ADR-0004) and register it for
    best-effort ``DELETE`` cleanup. Returns the raw response so callers can
    assert on the create response itself (status, ``Location``, ``ETag``).
    """

    def _create(
        headers: dict,
        ref: str,
        *,
        type_id: str | None = None,
        sharing: str = "tenant",
        value: str = "e2e-value",
        expires_at: str | None = None,
        fallback: str | None = None,
    ) -> httpx.Response:
        body: dict = {"sharing": sharing, "value": value}
        if type_id is not None:
            body["type"] = type_id
        if expires_at is not None:
            body["expires_at"] = expires_at
        if fallback is not None:
            body["fallback"] = fallback
        with httpx.Client(timeout=REQUEST_TIMEOUT) as client:
            resp = client.put(
                f"{credentials_url}/{ref}",
                headers={**headers, "If-None-Match": "*"},
                json=body,
            )
        cleanup(headers, ref)
        return resp

    return _create


@pytest.fixture(scope="session", autouse=True)
def _check_credstore_reachable():
    """Skip the whole suite when the e2e server is not running."""
    url = os.getenv("E2E_BASE_URL", "http://localhost:8086")
    try:
        # Any HTTP response (401/404 included) means the gateway is up.
        httpx.get(
            f"{url}/credstore/v1/credentials/e2e-reachability-probe",
            timeout=5.0,
        )
    except httpx.ConnectError:
        pytest.skip(f"e2e server not running at {url}", allow_module_level=True)
    except Exception:
        # Timeout or transient error — still try to run the tests.
        pass
