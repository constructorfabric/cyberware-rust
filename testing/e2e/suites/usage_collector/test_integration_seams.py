"""Integration-seam E2E tests for the usage-collector gear.

Each test targets exactly one seam that only manifests over real HTTP against
a real storage backend. Storage SQL, aggregation internals, domain validation
and DTO conversion are covered by unit tests and by
`make test-usage-collector-pg` — not here.

Backend-agnostic by construction: these bodies talk HTTP only, and
`make e2e-usage-collector` runs the whole module against both the TimescaleDB
and the ClickHouse storage plugin. Anything asserted here is therefore a
contract of the gear's API, not of one plugin — which is exactly what makes
the two plugins substitutable.
"""

from datetime import datetime
from decimal import Decimal

from .conftest import accepted_records, record_payload, window_filter


async def test_ingest_and_read_record_roundtrip(api, make_usage_type):
    """Seam: handler <-> JSON wire format <-> storage round-trip.

    A Decimal `value` crosses as a string and comes back with its numeric value
    intact, and an RFC3339 `created_at` survives the backend's timestamp
    round-trip (PostgreSQL `timestamptz` / ClickHouse `DateTime64(6)`).

    `value` is compared as a Decimal, not as a string: the trailing-zero scale
    is the storage column's, not the gear's. PostgreSQL `NUMERIC` keeps the
    scale it was given ("42.5"); ClickHouse `Decimal128(9)` is fixed-scale and
    returns "42.500000000". Both carry the same number, which is the contract
    the gear offers — asserting the digits would assert one plugin's column
    type.
    """
    gts_id, _ = await make_usage_type()
    payload = record_payload(gts_id, value="42.5")

    async with api() as client:
        created = accepted_records(
            await client.post("/records", json={"records": [payload]})
        )
        assert len(created) == 1
        record_id = created[0]["id"]

        fetched = await client.get(f"/records/{record_id}")

    assert fetched.status_code == 200, fetched.text
    body = fetched.json()
    assert body["id"] == record_id
    assert Decimal(str(body["value"])) == Decimal("42.5")
    assert body["gts_id"] == gts_id
    assert body["tenant_id"] == payload["tenant_id"]
    assert body["status"] == "active"
    assert body["resource_ref"]["resource_id"] == "res-1"
    assert datetime.fromisoformat(body["created_at"].replace("Z", "+00:00")) == \
        datetime.fromisoformat(payload["created_at"].replace("Z", "+00:00"))


async def test_list_records_odata_filter_and_cursor(api, make_usage_type):
    """Seam: OData $filter -> SQL, and the keyset cursor codec over HTTP.

    Following a cursor resends the IDENTICAL gts_id/$filter/limit — the
    toolkit rejects a cursor replayed under a different filter or order.
    `limit` is the page-size parameter: the toolkit's OData extractor
    (`ODataParams` in libs/toolkit/src/api/odata.rs) is what binds it to
    `ODataQuery.limit`.
    """
    gts_id, _ = await make_usage_type()
    async with api() as client:
        created = accepted_records(await client.post("/records", json={"records": [
            record_payload(gts_id, resource_id="res-a", value="1"),
            record_payload(gts_id, resource_id="res-b", value="2"),
        ]}))
        assert len(created) == 2
        all_ids = {r["id"] for r in created}

        params = {"gts_id": gts_id, "$filter": window_filter(), "limit": 1}

        first = await client.get("/records", params=params)
        assert first.status_code == 200, first.text
        page_one = first.json()
        assert len(page_one["items"]) == 1
        cursor = page_one["page_info"]["next_cursor"]
        assert cursor, "a second page exists, so next_cursor must be set"

        second = await client.get("/records", params={**params, "cursor": cursor})

    assert second.status_code == 200, second.text
    page_two = second.json()
    assert len(page_two["items"]) == 1

    seen = {page_one["items"][0]["id"], page_two["items"][0]["id"]}
    assert seen == all_ids, "the two pages must be disjoint and cover both records"


async def test_aggregate_groups_by_resource(api, make_usage_type):
    """Seam: aggregation request -> SQL GROUP BY -> PDP-scoped result.

    `sum` requires a counter usage type (it is a 400 on a gauge). group_by
    carries closed dimensions as bare snake_case strings.
    """
    gts_id, _ = await make_usage_type(kind="counter")
    async with api() as client:
        accepted_records(await client.post("/records", json={"records": [
            record_payload(gts_id, resource_id="res-a", value="10"),
            record_payload(gts_id, resource_id="res-a", value="5"),
            record_payload(gts_id, resource_id="res-b", value="7"),
        ]}))

        resp = await client.post(
            "/records/aggregate",
            params={"gts_id": gts_id, "$filter": window_filter()},
            json={"op": "sum", "group_by": ["resource_id"]},
        )

    assert resp.status_code == 200, resp.text
    buckets = resp.json()["buckets"]
    # `value` may arrive as a JSON string or number; Decimal(str(...)) accepts both.
    sums = {b["key"][0]: Decimal(str(b["value"])) for b in buckets}
    assert sums == {"res-a": Decimal("15"), "res-b": Decimal("7")}


async def test_deactivate_is_monotonic(api, make_usage_type):
    """Seam: deactivation is one-way, enforced at the storage transaction.

    First call flips active -> inactive (204). The second is REJECTED with 409
    `ALREADY_INACTIVE` — the store reads the row FOR UPDATE and refuses an
    already-inactive target. It is not an idempotent no-op (ADR-0005).
    """
    gts_id, _ = await make_usage_type()
    async with api() as client:
        created = accepted_records(
            await client.post("/records", json={"records": [record_payload(gts_id)]})
        )
        record_id = created[0]["id"]

        first = await client.post(f"/records/{record_id}/deactivate")
        assert first.status_code == 204, first.text

        after = await client.get(f"/records/{record_id}")
        assert after.status_code == 200, after.text
        assert after.json()["status"] == "inactive"

        second = await client.post(f"/records/{record_id}/deactivate")

    assert second.status_code == 409, f"expected 409, got {second.status_code}: {second.text}"
    assert second.headers["content-type"].startswith("application/problem+json")
    problem = second.json()
    assert problem["status"] == 409
    assert problem["context"]["reason"] == "ALREADY_INACTIVE"


async def test_list_usage_types_includes_created(api, make_usage_type):
    """Seam: catalog list projects plugin-owned rows onto the wire.

    `limit` is the page-size parameter here too: this endpoint shares the
    toolkit `OData` extractor with `/records`. A large `limit` keeps this
    assertion stable as the session accumulates usage types across tests;
    1000 is the documented ceiling and the plugin's clamp.
    """
    gts_id, metadata_key = await make_usage_type()
    async with api() as client:
        resp = await client.get("/usage-types", params={"limit": 1000})

    assert resp.status_code == 200, resp.text
    by_id = {item["gts_id"]: item for item in resp.json()["items"]}
    assert gts_id in by_id, f"{gts_id} missing from the catalog listing"
    assert by_id[gts_id]["kind"] == "counter"
    assert by_id[gts_id]["metadata_fields"] == [metadata_key]


async def test_get_usage_type(api, make_usage_type):
    """Seam: single catalog read by GTS id in the path.

    The id contains `~` and `.`; this also proves the path segment survives
    routing without mangling.
    """
    gts_id, metadata_key = await make_usage_type()
    async with api() as client:
        resp = await client.get(f"/usage-types/{gts_id}")

    assert resp.status_code == 200, resp.text
    body = resp.json()
    assert body["gts_id"] == gts_id
    assert body["kind"] == "counter"
    assert body["metadata_fields"] == [metadata_key]


async def test_delete_usage_type_referenced_by_record_is_rejected(api, make_usage_type):
    """Seam: referential integrity surfaced as HTTP 409.

    No unit test can observe this: the constraint lives in the storage
    backend, and each plugin enforces it differently — TimescaleDB with a real
    FK `ON DELETE RESTRICT` (SQLite's default FK behaviour differs), ClickHouse
    with a lock-protected verify-then-delete, since it has no FKs at all. The
    gear's contract is identical either way, which is what this asserts: an
    unreferenced type deletes cleanly (204), a referenced one is refused (409).
    """
    referenced_id, _ = await make_usage_type()
    unreferenced_id, _ = await make_usage_type()

    async with api() as client:
        accepted_records(await client.post(
            "/records", json={"records": [record_payload(referenced_id)]}
        ))

        blocked = await client.delete(f"/usage-types/{referenced_id}")
        allowed = await client.delete(f"/usage-types/{unreferenced_id}")

    assert blocked.status_code == 409, (
        f"a referenced usage type must not be deletable, got "
        f"{blocked.status_code}: {blocked.text}"
    )
    assert blocked.headers["content-type"].startswith("application/problem+json")
    # Reason code from ConflictReason::UsageTypeReferenced (usage-collector-sdk/src/reason.rs),
    # constructed by UsageCollectorError::usage_type_referenced (usage-collector-sdk/src/error.rs)
    # and lifted onto `context.reason` in usage-collector/src/infra/sdk_error_mapping.rs. Asserting
    # on it (not just the status code) keeps this test tied to the real FK RESTRICT seam: an
    # application-level pre-check returning a bare 409 for a different reason would stay green
    # on `status_code == 409` alone.
    assert blocked.json()["context"]["reason"] == "USAGE_TYPE_REFERENCED"
    assert allowed.status_code == 204, allowed.text
