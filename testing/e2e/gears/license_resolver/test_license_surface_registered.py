"""E2E tests for the license resolver's GTS surface.

The resolver exposes no REST API — its only operation is the in-process
``is_licensed`` check — so what an E2E can observe is the surface it publishes
into types-registry, and the fact that the server boots at all with it linked.

That boot is the point. types-registry commits every linked crate's GTS
inventory in one all-or-nothing pass during ``post_init``: a licensing type or a
plugin instance that fails validation stops the whole server from binding, not
just licensing. These tests fail loudly on that, instead of leaving every other
suite to fail with an unexplained startup timeout.

Grant and denial semantics are covered in-process by the plugin's own tests
(``cargo test -p cf-gears-static-license-plugin``); they are unreachable from
HTTP by design.
"""
import httpx
import pytest

SUBJECT_BASE = "gts.cf.core.lic.subj.v1~"
RESOURCE_BASE = "gts.cf.core.lic.res.v1~"
PLUGIN_SPEC = "gts.cf.toolkit.plugins.plugin.v1~cf.core.license_resolver.plugin.v1~"
STATIC_PLUGIN_INSTANCE = PLUGIN_SPEC + "cf.builtin.static_license_resolver.plugin.v1"


async def _get_entity(client, base_url, auth_headers, gts_id):
    response = await client.get(
        f"{base_url}/types-registry/v1/entities/{gts_id}",
        headers=auth_headers,
    )
    if response.status_code in (401, 403) and not auth_headers:
        pytest.skip(
            f"Endpoint requires authentication (got {response.status_code}). "
            "Set E2E_AUTH_TOKEN environment variable to run this test."
        )
    assert response.status_code == 200, (
        f"{gts_id} must be registered after startup: "
        f"{response.status_code} {response.text}"
    )
    body = response.json()
    return body.get("content", body)


@pytest.mark.smoke
@pytest.mark.asyncio
async def test_licensing_base_types_are_registered(base_url, auth_headers):
    """Both licensing bases reach types-registry via the link-time inventory.

    Consuming Gears derive their contracts from these, so an absent base makes
    every derived contract unregisterable.
    """
    async with httpx.AsyncClient(timeout=10.0) as client:
        for gts_id in (SUBJECT_BASE, RESOURCE_BASE):
            schema = await _get_entity(client, base_url, auth_headers, gts_id)
            assert schema.get("x-gts-abstract") is True, (
                f"{gts_id} must be abstract — only derived contracts are "
                f"instantiable: {schema}"
            )


@pytest.mark.asyncio
async def test_static_plugin_instance_is_discoverable(base_url, auth_headers):
    """The backend advertises itself with the vendor the gateway selects on.

    Requires ``--features static-license``. Without a discoverable instance the
    gateway has no backend and every check fails closed.
    """
    async with httpx.AsyncClient(timeout=10.0) as client:
        await _get_entity(client, base_url, auth_headers, PLUGIN_SPEC)
        instance = await _get_entity(client, base_url, auth_headers, STATIC_PLUGIN_INSTANCE)

    assert instance.get("vendor") == "constructorfabric", (
        f"the instance must carry the vendor the resolver is configured with: {instance}"
    )
    assert isinstance(instance.get("priority"), int), instance
