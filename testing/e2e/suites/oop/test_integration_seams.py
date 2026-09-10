"""Out-of-process (loopback) integration seams.

Each test targets exactly one cross-process seam that only manifests when the
platform-host and the gears run as separate processes wired by the
DirectoryService — none is reachable by a unit test or an in-process E2E.

Topology (booted by conftest.oop_cluster):
    platform-host   edge :8087  + DirectoryService :50051
    hello-oop       :9091   (anonymous REST)
    api-contracts-oop            :9097  (PaymentApi REST provider)
    api-contracts-consumer-oop   :9098  (resolves PaymentApi from the provider)
"""
from __future__ import annotations

import httpx
import pytest

TIMEOUT = 5.0


@pytest.mark.smoke
def test_edge_healthz(oop_cluster):
    """Seam: the platform-host edge is up and serving."""
    r = httpx.get(f"{oop_cluster}/healthz", timeout=TIMEOUT)
    assert r.status_code == 200, r.text


@pytest.mark.smoke
def test_hello_anonymous_cross_process_proxy(oop_cluster):
    """Seam: edge reverse-proxies an anonymous route to a separate gear process.

    `served_by` is the serving process id — proof the request crossed the
    process boundary (edge -> hello-oop) rather than being handled in-process.
    """
    r = httpx.get(f"{oop_cluster}/hello/v1/ping", timeout=TIMEOUT)
    assert r.status_code == 200, r.text
    body = r.json()
    assert body.get("message") == "pong", body
    assert "hello-oop" in str(body.get("served_by", "")), body


def test_missing_bearer_rejected_at_edge(oop_cluster):
    """Seam: tenant-plane gating — an authenticated route needs a bearer."""
    r = httpx.post(
        f"{oop_cluster}/api-contracts-consumer/v1/charge",
        json={"amount_cents": 1000, "currency": "USD", "description": "no-token"},
        timeout=TIMEOUT,
    )
    assert r.status_code == 401, f"expected 401, got {r.status_code}: {r.text}"


@pytest.mark.smoke
def test_oop_to_oop_charge_over_rest(oop_cluster, auth):
    """Seam: OoP -> OoP contract call over REST, discovered via the directory.

    A single edge request drives ingress -> consumer pod -> provider pod: the
    consumer resolves `PaymentApi` from the SEPARATE provider process (its
    binary does not link the provider), so the charge can only travel over REST.
    """
    r = httpx.post(
        f"{oop_cluster}/api-contracts-consumer/v1/charge",
        headers={**auth, "Content-Type": "application/json"},
        json={"amount_cents": 1000, "currency": "USD", "description": "oop-e2e charge"},
        timeout=TIMEOUT,
    )
    assert r.status_code == 200, f"expected 200, got {r.status_code}: {r.text}"
    body = r.json()
    assert body.get("payment_id"), body
    assert body.get("status") == "pending", body


def test_provider_reachable_only_through_consumer(oop_cluster, auth):
    """Seam: the provider's PaymentApi charge route is not edge-exposed.

    The provider executes charges (verified via the consumer path above), but
    its charge route (`POST /api-contracts/v1/payments/charge`) is not marked
    `.exposed()`, so the edge never syncs it into its route table — only the
    consumer's own `.exposed()` route is published. With a valid bearer the
    edge auth layer passes and the unknown path falls through the proxy
    fallback, which returns 404 ("no upstream route registered").
    """
    r = httpx.post(
        f"{oop_cluster}/api-contracts/v1/payments/charge",
        headers={**auth, "Content-Type": "application/json"},
        json={"amount_cents": 1000, "currency": "USD", "description": "direct"},
        timeout=TIMEOUT,
    )
    assert r.status_code == 404, (
        f"provider charge should not be edge-exposed, got {r.status_code}: {r.text}"
    )
