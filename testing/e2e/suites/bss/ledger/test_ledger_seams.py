"""Black-box E2E seam tests for the bss-ledger gear.

Drives the running ``cf-gears-server`` over HTTP (no in-process access). The
whole module is skipped by ``conftest.require_ledger_mounted`` until the gear's
routes are mounted, so these assert reachability + the coarse REST contract
(auth gate, scoped not-found, route presence) rather than deep payloads — the
full posting/allocation behaviour is covered by the crate's Postgres
integration tests.
"""

import uuid

import httpx
import pytest

from .conftest import TENANT_A_ID, TENANT_B_ID

REQUEST_TIMEOUT = 10.0

# The tenant the standard E2E token (`e2e-token-tenant-a`) authenticates as —
# its `subject_tenant_id` in config/e2e-local.yaml's static-authn-plugin. Reads
# are tenant-scoped, so the seam tests name this (in-scope) id: an absent
# resource then reads as a clean 404 (not a scope miss), and list/read surfaces
# resolve to the caller's own — possibly empty — data. Read-by-id takes it as
# `?tenant_id=`; collection GETs are OData-only and take `$filter=tenant_id eq
# <uuid>` instead — a named key there is a 400.
TENANT_A = TENANT_A_ID
# The foreign tenant used by the cross-tenant no-existence-leak tests — a
# different root the token `e2e-token-tenant-b` authenticates as (see conftest).
TENANT_B = TENANT_B_ID

# Write surfaces (POST) whose route presence + auth gate we probe. Full paths
# under the gear's `/bss-ledger/v1` base (design §3.3 / the axum routers).
WRITE_PATHS = [
    "/journal-entries",
    "/payments",
    "/credit-notes",
    "/debit-notes",
    "/manual-adjustments",
    "/refunds",
    "/recognition-runs",
]

# Read-by-id surfaces (GET) — an absent/foreign id must read as a scoped 404.
GET_BY_ID_PATHS = [
    "/journal-entries/{id}",
    "/recognition-schedules/{id}",
    "/refunds/{id}",
    "/credit-notes/{id}",
]


def test_accounts_read_is_reachable(base_url, auth_headers, api_base):
    """The chart-of-accounts read surface is mounted and returns JSON.

    (The session probe already proved non-404; here we assert the authenticated
    read succeeds and yields a JSON body — the paginated chart envelope.)
    """
    with httpx.Client(timeout=REQUEST_TIMEOUT) as client:
        r = client.get(
            f"{base_url}{api_base}/accounts?$filter=tenant_id%20eq%20{TENANT_A}",
            headers=auth_headers,
        )
    assert r.status_code == 200, f"expected 200, got {r.status_code}: {r.text}"
    # Either the canonical page envelope or a bare list — both are valid JSON.
    assert isinstance(r.json(), (dict, list))


def test_journal_entry_not_leaked_across_tenants(
    base_url, auth_headers, auth_headers_tenant_b, api_base, seeded_entry
):
    """A journal entry seeded in TENANT_A's ledger is invisible to TENANT_B.

    Genuine cross-tenant BOLA / no-existence-leak. The `seeded_entry` fixture
    posts a REAL entry as TENANT_A; TENANT_B then reads that SAME id and MUST get
    the same 404 it gets for a random id — existence is not leaked across tenants.

    This replaces the earlier check that issued a random uuid under a SINGLE
    tenant and asserted 404: that proved the absent→404 mapping but nothing about
    cross-tenant isolation (a random id is trivially absent for everyone). Here
    the id demonstrably EXISTS (the owner baseline below is 200), so TENANT_B's
    404 is a true isolation signal.
    """
    with httpx.Client(timeout=REQUEST_TIMEOUT) as client:
        # Baseline: the owner (TENANT_A) DOES see the entry — it genuinely exists,
        # so the cross-tenant 404 below cannot be an accident of an absent id.
        owner = client.get(
            f"{base_url}{api_base}/journal-entries/{seeded_entry}?tenant_id={TENANT_A}",
            headers=auth_headers,
        )
        assert owner.status_code == 200, (
            f"owner (TENANT_A) must see the seeded entry, "
            f"got {owner.status_code}: {owner.text}"
        )

        # Cross-tenant: TENANT_B reads the SAME real id within its own scope — the
        # foreign entry resolves to None ⇒ 404 (no existence leak).
        cross = client.get(
            f"{base_url}{api_base}/journal-entries/{seeded_entry}?tenant_id={TENANT_B}",
            headers=auth_headers_tenant_b,
        )
        assert cross.status_code == 404, (
            f"TENANT_B must NOT see TENANT_A's entry (expected 404), "
            f"got {cross.status_code}: {cross.text}"
        )

        # Impersonation: TENANT_B explicitly targets TENANT_A's scope by passing
        # tenant_id=TENANT_A. The PEP subtree clamp still hides the row — either a
        # 404 (no-existence-leak) or a 403 (hard scope deny) is acceptable; a 200
        # would be the BOLA break.
        impersonate = client.get(
            f"{base_url}{api_base}/journal-entries/{seeded_entry}?tenant_id={TENANT_A}",
            headers=auth_headers_tenant_b,
        )
        assert impersonate.status_code in (403, 404), (
            f"TENANT_B targeting TENANT_A's scope must be denied (403/404), "
            f"got {impersonate.status_code}: {impersonate.text}"
        )

        # Equivalence: a random id under TENANT_B is ALSO a 404 — proving the
        # foreign REAL id is indistinguishable from a never-existed one.
        random_404 = client.get(
            f"{base_url}{api_base}/journal-entries/{uuid.uuid4()}?tenant_id={TENANT_B}",
            headers=auth_headers_tenant_b,
        )
        assert random_404.status_code == cross.status_code == 404, (
            "a foreign real id and a random id must both read as 404 "
            f"(random={random_404.status_code}, cross={cross.status_code})"
        )


def test_post_journal_entry_requires_auth(base_url, api_base):
    """Posting without a bearer token is rejected by the gateway auth gate (401).

    No ``auth_headers`` here on purpose: the write surface must never accept an
    unauthenticated post.
    """
    with httpx.Client(timeout=REQUEST_TIMEOUT) as client:
        r = client.post(f"{base_url}{api_base}/journal-entries", json={})
    assert r.status_code == 401, f"expected 401, got {r.status_code}: {r.text}"


def test_provisioning_route_is_present(base_url, auth_headers, api_base):
    """The seller-provisioning route exists (a bad body is a 4xx, never a 404).

    An empty body fails request validation / the seller predicate, so we expect
    some client error — the point is only that the route is MOUNTED (not 404),
    the one Foundation endpoint the design marks externally exposed.
    """
    with httpx.Client(timeout=REQUEST_TIMEOUT) as client:
        r = client.post(
            f"{base_url}{api_base}/provisioning",
            headers=auth_headers,
            json={},
        )
    assert r.status_code != 404, f"provisioning route must be mounted: {r.text}"
    assert 400 <= r.status_code < 500, (
        f"an empty provisioning body should be a 4xx, got {r.status_code}: {r.text}"
    )


def test_balances_read_is_reachable(base_url, auth_headers, api_base):
    """The current-balances read surface is mounted (no server error).

    A bare read may 200 (empty) or 400 (a required filter is missing), but never
    404 (route absent) or 5xx (server fault).
    """
    with httpx.Client(timeout=REQUEST_TIMEOUT) as client:
        r = client.get(
            f"{base_url}{api_base}/balances?$filter=tenant_id%20eq%20{TENANT_A}",
            headers=auth_headers,
        )
    assert r.status_code != 404, f"balances route must be mounted: {r.text}"
    assert r.status_code < 500, f"balances read must not 5xx: {r.status_code} {r.text}"


def test_journal_entries_list_is_reachable(base_url, auth_headers, api_base):
    """The journal-entries list surface returns a JSON page envelope."""
    with httpx.Client(timeout=REQUEST_TIMEOUT) as client:
        r = client.get(
            f"{base_url}{api_base}/journal-entries?$filter=tenant_id%20eq%20{TENANT_A}",
            headers=auth_headers,
        )
    assert r.status_code == 200, f"expected 200, got {r.status_code}: {r.text}"
    assert isinstance(r.json(), (dict, list))


@pytest.mark.parametrize("path", WRITE_PATHS)
def test_write_routes_require_auth(base_url, api_base, path):
    """Every write surface is behind the gateway auth gate — no token ⇒ 401.

    No ``auth_headers`` on purpose: an unauthenticated POST to a write route
    must never be accepted.
    """
    with httpx.Client(timeout=REQUEST_TIMEOUT) as client:
        r = client.post(f"{base_url}{api_base}{path}", json={})
    assert r.status_code == 401, (
        f"{path}: expected 401 without a token, got {r.status_code}: {r.text}"
    )


@pytest.mark.parametrize("path", WRITE_PATHS)
def test_write_routes_are_mounted(base_url, auth_headers, api_base, path):
    """Every write surface is mounted: an empty body is a 4xx, never a 404/5xx.

    Proves the route exists (request validation / a domain guard rejects the
    empty body) without asserting the exact rejection code.
    """
    with httpx.Client(timeout=REQUEST_TIMEOUT) as client:
        r = client.post(f"{base_url}{api_base}{path}", headers=auth_headers, json={})
    assert r.status_code != 404, f"{path}: route must be mounted: {r.text}"
    assert 400 <= r.status_code < 500, (
        f"{path}: an empty body should be a 4xx, got {r.status_code}: {r.text}"
    )


@pytest.mark.parametrize("path", GET_BY_ID_PATHS)
def test_get_by_id_absent_is_404(base_url, auth_headers, api_base, path):
    """An id the caller's own tenant never created reads as a scoped 404.

    A route-contract check: each read-by-id surface is mounted and maps an absent
    id to a clean 404 (not a 500 or a router-miss). This does NOT by itself prove
    cross-tenant isolation — a random id is trivially absent for everyone. The
    genuine no-existence-leak (BOLA) property is covered by
    ``test_journal_entry_not_leaked_across_tenants``, which seeds a REAL entry as
    one tenant and asserts another tenant gets the same 404.
    """
    url = f"{base_url}{api_base}{path.format(id=uuid.uuid4())}?tenant_id={TENANT_A}"
    with httpx.Client(timeout=REQUEST_TIMEOUT) as client:
        r = client.get(url, headers=auth_headers)
    assert r.status_code == 404, f"{path}: expected 404, got {r.status_code}: {r.text}"


def test_not_found_is_problem_json(base_url, auth_headers, api_base):
    """The gear renders a not-found as RFC 9457 ``application/problem+json``.

    Checks the error envelope the gear's canonical-error middleware produces
    (content type + a ``status`` of 404 in the body), the machine-readable shape
    consumers match on.
    """
    url = f"{base_url}{api_base}/journal-entries/{uuid.uuid4()}?tenant_id={TENANT_A}"
    with httpx.Client(timeout=REQUEST_TIMEOUT) as client:
        r = client.get(url, headers=auth_headers)
    assert r.status_code == 404
    assert "problem+json" in r.headers.get("content-type", ""), (
        f"expected application/problem+json, got {r.headers.get('content-type')!r}"
    )
    body = r.json()
    assert body.get("status") == 404, f"problem body should carry status 404: {body}"
