"""E2E integration seam tests for the credstore gear (ADR-0004: the
credential surface; ADR-0005: the collection read).

One file, each endpoint exercised at most once across a happy path per
seam. Negative/error paths appear only where the failure is itself an
integration seam (a live 400/404 through the real router + domain
service), per ``docs/toolkit_unified_system/13_e2e_testing.md``.
"""
from __future__ import annotations

import re

import httpx
import pytest

from .conftest import REQUEST_TIMEOUT

GENERIC_TYPE = "gts.cf.core.credstore.credential.v1~cf.core.credstore.generic.v1~"
PERSONAL_TOKEN_TYPE = (
    "gts.cf.core.credstore.credential.v1~cf.core.credstore.personal_token.v1~"
)

ETAG_RE = re.compile(r'^"([0-9a-f-]{36})\.(\d+)"$')

CREDENTIAL_DTO_KEYS = {
    "reference",
    "type",
    "sharing",
    "fallback",
    "status",
    "inheritance",
    "version",
    "updated_at",
    "expires_at",
    "owner_id",
}


def _credential(ref: str) -> str:
    return f"/credstore/v1/credentials/{ref}"


def _secret(ref: str) -> str:
    return f"/credstore/v1/credentials/{ref}/secret"


# ── S1: Route smoke ─────────────────────────────────────────────────────


@pytest.mark.smoke
async def test_route_smoke_list_credentials(base_url, l1a_headers):
    """Seam: route registration — `GET /credentials` is mounted and returns
    a JSON page, not 404/405."""
    async with httpx.AsyncClient(timeout=REQUEST_TIMEOUT) as c:
        r = await c.get(f"{base_url}/credstore/v1/credentials", headers=l1a_headers)
    assert r.status_code == 200
    assert "application/json" in r.headers.get("content-type", "")
    body = r.json()
    assert isinstance(body["items"], list)
    assert "page_info" in body


# ── S2: Create -> read -> read secret -> rotate -> list (metadata + value
#       mode) -> suppress -> delete: one credential's full lifecycle ────────


async def test_credential_lifecycle_and_listing_seam(
    base_url, l1a_headers, unique_ref, create_credential,
):
    """Seam: PUT create-only -> GET record -> GET secret -> PATCH rotate ->
    collection read (metadata and value mode) -> PATCH suppress -> DELETE.

    One reference walks every write/read address the credential surface
    has, plus both collection-read modes, so each is exercised exactly once
    in a single coherent story.
    """
    ref = unique_ref("lifecycle")

    # --- PUT (create-only): 201, Location, strong ETag ---
    create_resp = create_credential(
        l1a_headers,
        ref,
        type_id=PERSONAL_TOKEN_TYPE,
        sharing="private",
        value="initial-value",
        expires_at="2099-01-01T00:00:00Z",
    )
    assert create_resp.status_code == 201, create_resp.text
    assert create_resp.headers["location"].endswith(_credential(ref))
    create_etag = create_resp.headers["etag"]
    assert ETAG_RE.match(create_etag), f"not a strong ETag: {create_etag}"

    # --- GET record: 200, no-store, exact DTO key set, owner_id present ---
    async with httpx.AsyncClient(timeout=REQUEST_TIMEOUT) as c:
        get_resp = await c.get(f"{base_url}{_credential(ref)}", headers=l1a_headers)
    assert get_resp.status_code == 200
    assert get_resp.headers.get("cache-control") == "no-store"
    body = get_resp.json()
    assert set(body.keys()) == CREDENTIAL_DTO_KEYS, sorted(body.keys())
    assert body["reference"] == ref
    assert body["type"] == PERSONAL_TOKEN_TYPE
    assert body["sharing"] == "private"
    assert body["status"] == "active"
    assert body["inheritance"] == "own"
    assert body["fallback"] == "inherit"
    assert body["version"] == 1
    assert body["expires_at"] == "2099-01-01T00:00:00Z"
    assert body["owner_id"] is not None

    # --- GET secret: 200, value round-trips, type, expires_at ---
    async with httpx.AsyncClient(timeout=REQUEST_TIMEOUT) as c:
        secret_resp = await c.get(f"{base_url}{_secret(ref)}", headers=l1a_headers)
    assert secret_resp.status_code == 200
    secret_body = secret_resp.json()
    assert secret_body["value"] == "initial-value"
    assert secret_body["type"] == PERSONAL_TOKEN_TYPE
    assert secret_body["expires_at"] == "2099-01-01T00:00:00Z"

    # --- PATCH: rotate the value (merge-patch, If-Match) -> 204, new ETag ---
    async with httpx.AsyncClient(timeout=REQUEST_TIMEOUT) as c:
        patch_resp = await c.patch(
            f"{base_url}{_credential(ref)}",
            headers={
                **l1a_headers,
                "Content-Type": "application/merge-patch+json",
                "If-Match": create_etag,
            },
            json={"value": "rotated-value"},
        )
    assert patch_resp.status_code == 204
    rotated_etag = patch_resp.headers["etag"]
    assert rotated_etag != create_etag

    # --- Collection read, metadata mode: item present, no `secret` key ---
    async with httpx.AsyncClient(timeout=REQUEST_TIMEOUT) as c:
        list_resp = await c.get(
            f"{base_url}/credstore/v1/credentials",
            headers=l1a_headers,
            params={"$filter": f"reference eq '{ref}'"},
        )
    assert list_resp.status_code == 200
    items = list_resp.json()["items"]
    assert len(items) == 1, items
    assert items[0]["reference"] == ref
    assert "secret" not in items[0]

    # --- Collection read, value mode: item carries the rotated secret ---
    async with httpx.AsyncClient(timeout=REQUEST_TIMEOUT) as c:
        value_mode_resp = await c.get(
            f"{base_url}/credstore/v1/credentials",
            headers=l1a_headers,
            params={
                "$select": "reference,secret",
                "$filter": f"reference eq '{ref}'",
            },
        )
    assert value_mode_resp.status_code == 200
    value_items = value_mode_resp.json()["items"]
    assert len(value_items) == 1, value_items
    assert value_items[0]["reference"] == ref
    assert value_items[0]["secret"] == "rotated-value"

    # --- PATCH: suppress (fallback: none, value: null) -> 204 ---
    async with httpx.AsyncClient(timeout=REQUEST_TIMEOUT) as c:
        suppress_resp = await c.patch(
            f"{base_url}{_credential(ref)}",
            headers={
                **l1a_headers,
                "Content-Type": "application/merge-patch+json",
                "If-Match": rotated_etag,
            },
            json={"fallback": "none", "value": None},
        )
    assert suppress_resp.status_code == 204

    # --- GET secret now 404 (no value); GET record: declared/suppressed ---
    async with httpx.AsyncClient(timeout=REQUEST_TIMEOUT) as c:
        secret_after_suppress = await c.get(
            f"{base_url}{_secret(ref)}", headers=l1a_headers
        )
        record_after_suppress = await c.get(
            f"{base_url}{_credential(ref)}", headers=l1a_headers
        )
    assert secret_after_suppress.status_code == 404
    suppressed_body = record_after_suppress.json()
    assert suppressed_body["status"] == "declared"
    assert suppressed_body["inheritance"] == "suppressed"

    # --- DELETE -> 204, then GET -> 404 ---
    async with httpx.AsyncClient(timeout=REQUEST_TIMEOUT) as c:
        delete_resp = await c.delete(
            f"{base_url}{_credential(ref)}",
            headers={**l1a_headers, "If-Match": "*"},
        )
        gone_resp = await c.get(f"{base_url}{_credential(ref)}", headers=l1a_headers)
    assert delete_resp.status_code == 204
    assert gone_resp.status_code == 404


# ── S3: Tenant isolation (upward-only inheritance, ADR-0005) ────────────────


async def test_tenant_isolation_seam(
    base_url, root_headers, l1a_headers, tenant_a_headers, unique_ref, create_credential,
):
    """Seam: a `shared` credential published by hierarchy-root is inherited
    by its descendant hierarchy-l1a, but invisible to hierarchy-root's own
    ancestor (e2e-root / tenant_a) — resolution walks upward only, so a
    tenant above the publisher never sees what was shared below it.
    """
    ref = unique_ref("isolation")
    create_resp = create_credential(
        root_headers,
        ref,
        type_id=GENERIC_TYPE,
        sharing="shared",
        value="shared-secret",
    )
    assert create_resp.status_code == 201, create_resp.text

    async with httpx.AsyncClient(timeout=REQUEST_TIMEOUT) as c:
        inherited_resp = await c.get(f"{base_url}{_credential(ref)}", headers=l1a_headers)
        invisible_resp = await c.get(
            f"{base_url}{_credential(ref)}", headers=tenant_a_headers
        )

    assert inherited_resp.status_code == 200
    inherited_body = inherited_resp.json()
    assert inherited_body["inheritance"] == "inherited"
    assert inherited_body["sharing"] == "shared"
    assert "owner_id" not in inherited_body

    assert invisible_resp.status_code == 404


# ── S4: Error shape (RFC 9457) ──────────────────────────────────────────────


async def test_error_response_is_problem_json(base_url, l1a_headers, unique_ref):
    """Seam: a domain validation failure (PUT without `value`) renders as
    `application/problem+json`, not a generic framework error."""
    ref = unique_ref("badput")
    async with httpx.AsyncClient(timeout=REQUEST_TIMEOUT) as c:
        r = await c.put(
            f"{base_url}{_credential(ref)}",
            headers={**l1a_headers, "If-None-Match": "*"},
            json={"sharing": "tenant"},
        )
    assert r.status_code == 400
    assert "application/problem+json" in r.headers.get("content-type", "")
    body = r.json()
    assert body.get("status") == 400
    assert "stack" not in body
    assert "trace" not in body
    assert "backtrace" not in body
