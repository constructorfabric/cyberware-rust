"""Bundles, overlays, and the preconditions the lifecycle verbs actually enforce.

These sit beside :mod:`test_pricing_seams`, which owns the plan and publish path.
Everything here is a surface that module does not reach, and every case was
written because building a client against these routes surfaced a fact the
contract did not state — or stated somewhere a caller does not read.

# What a caller cannot learn from `openapi.json`

Three of the assertions below exist because the published document is silent or
wrong about the request:

* ``POST /bundles/{bundleId}/publish`` declares **no request body** while the
  handler reads ``PublishBundleRequest { plan_revision, markets }``. A caller
  that believes the document sends nothing and is refused for a missing field.
* ``POST /bundles`` and ``PATCH /bundles/{bundleId}`` likewise declare none;
  the first reads ``CreateBundleRequest``, the second a whole
  ``CompositionRequest``, and the second also requires ``If-Match``.
* ``GET /price-overlays`` answers ``{overlays, page_info}`` — not the ``items``
  envelope every other list on this gear uses.

Pinning them here is what makes the document's gaps cost one failing test rather
than a client rewrite.

# The lifecycle verbs and their precondition

``POST /plans/{planId}/abandon`` **requires** ``If-Match``, and its URL ends in
the verb rather than in the plan id. A client that keys its validators by entity
id therefore attaches nothing and is refused — which is exactly what happened to
the pricing MFE, silently, until an end-to-end run put the gear's own sentence on
the screen. :func:`test_abandon_without_a_precondition_is_refused_by_its_code`
is that failure, pinned at the wire where the rule lives.
"""

import uuid

import pytest

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _problem_codes(payload: dict) -> set:
    """Every machine-readable code in an RFC 9457 problem document.

    Copied deliberately rather than imported from the sibling module: these two
    files are read on their own, and §3.3 makes the **code** the discriminator a
    consumer matches on, so the helper that reads it belongs with the assertions
    that use it.
    """
    found = set()

    def walk(node):
        if isinstance(node, dict):
            for key in ("reason", "type", "code"):
                value = node.get(key)
                if isinstance(value, str) and value.isupper() and len(value) > 3:
                    found.add(value)
            for value in node.values():
                walk(value)
        elif isinstance(node, list):
            for item in node:
                walk(item)

    walk(payload)
    return found


def _violations(payload: dict) -> list:
    """The precondition violations a refusal carries, as descriptions."""
    context = payload.get("context") or {}
    return [v.get("description", v.get("type", "")) for v in context.get("violations", [])]


def _declare_region(client, headers, region="EU"):
    """Declare a region so a price row may be keyed on it.

    ``REGION_UNKNOWN`` (C2): a row's ``region`` must be an **active** value of the
    tenant's region taxonomy, checked at save and again at publish. So a price row
    needs two things this module has to arrange before it can exist — a phase on
    the plan and a declared region — and neither is implied by the create body.

    Declared through ``POST .../taxonomies/region/values`` (D-353): the value is
    its own key, so the first call answers 201 and every repeat with the same body
    answers 200 as the create's replay. Idempotent, so every case may call it.
    """
    declared = client.post(
        "/bss-pricing/v1/config/taxonomies/region/values",
        headers=headers,
        json={
            "value": region,
            "display_name": region,
            "state": "active",
            "tax_category": "standard",
            "tax_rate_present": True,
        },
    )
    assert declared.status_code in (200, 201), declared.text
    return region


def _declare_customer_group(client, headers, value="gold-partners"):
    """Declare a customer group so a `customer_group`-scoped overlay may name it.

    ``SCOPE_VALUE_UNKNOWN`` (D-120): the scope value of such an overlay selects
    **who** receives the adjustment, so it has to exist in the tenant's declared
    universe. Checked at submit rather than at create, like every other rule in
    ``overlay_rules::validate``.
    """
    read = client.get("/bss-pricing/v1/customer-groups/taxonomy", headers=headers)
    assert read.status_code == 200, read.text
    put = client.put(
        "/bss-pricing/v1/customer-groups/taxonomy",
        headers={**headers, "If-Match": read.headers["etag"]},
        json={"values": [{"value": value, "display_name": value, "state": "active"}]},
    )
    # Not `in (200, 409)`: tolerating a conflict here hid a taxonomy that was
    # never declared, and the case that depended on it failed three steps later
    # with a message about the overlay.
    assert put.status_code == 200, put.text

    # Read it back before relying on it: the case that needs this used to fail
    # three steps later, with a message about the overlay, when the declaration
    # had not landed.
    back = client.get("/bss-pricing/v1/customer-groups/taxonomy", headers=headers)
    assert back.status_code == 200, back.text
    declared = {v["value"]: v.get("state") for v in back.json()["values"]}
    assert value in declared, (
        f"the customer group was written and is not readable back: {back.text}"
    )
    assert declared[value] in (None, "active"), declared
    return value


def _new_plan(client, headers, *, tier="gold", with_phase=True):
    """A draft plan, with one evergreen phase unless asked otherwise.

    The phase is not decoration. ``POST /plans`` takes a ``PlanShapeRequest``,
    which has **no** ``phases`` member — the chain travels in
    ``PatchPlanRequest`` — and a price row's scope key names a phase the gear
    parses as a UUID. So a plan created and left alone can be authored and then
    never priced, which is the shape
    :func:`test_a_row_naming_a_phase_the_plan_does_not_schedule_is_refused`
    pins from the other side.

    Returns ``(plan_id, phase_id, etag)``; ``phase_id`` is ``None`` when the plan
    was left without one.
    """
    created = client.post(
        "/bss-pricing/v1/plans",
        headers={**headers, "Idempotency-Key": str(uuid.uuid4())},
        json={"plan_tier": tier, "billing_cycle": "recurring"},
    )
    assert created.status_code == 201, created.text
    plan_id = created.json()["plan_id"]
    etag = created.headers["etag"]
    if not with_phase:
        return plan_id, None, etag

    phase_id = str(uuid.uuid4())
    phased = client.patch(
        f"/bss-pricing/v1/plans/{plan_id}",
        headers={**headers, "If-Match": etag},
        json={
            "phases": [
                {
                    "phase_id": phase_id,
                    "kind": "evergreen",
                    "ordinal": 0,
                    "converts_to_phase_id": None,
                    "phase_duration_days": None,
                    "display_trial_days": None,
                }
            ]
        },
    )
    assert phased.status_code == 200, phased.text
    return plan_id, phase_id, phased.headers["etag"]


def _add_row(
    client,
    headers,
    plan_id,
    phase_id,
    content,
    *,
    currency="USD",
    region="EU",
    charge_kind="recurring",
):
    """Author a price row.

    ``charge_kind`` is a parameter and not a constant because a row carrying a
    ``meter`` **must** be keyed ``usage``: the meter and the dimension key are
    axes of the canonical key, so a metered row on a recurring key is refused
    ``USAGE_LINE_AXIS_MISMATCH``. The two travel together or not at all.
    """
    return client.post(
        f"/bss-pricing/v1/plans/{plan_id}/prices",
        headers={**headers, "Idempotency-Key": str(uuid.uuid4())},
        json={
            "scope_key": {
                "currency": currency,
                "region": region,
                "phase": phase_id,
                "price_eligibility": "all_subscriptions",
                "charge_kind": charge_kind,
                "cohort": None,
            },
            "content": content,
        },
    )


# ---------------------------------------------------------------------------
# The lifecycle verbs' precondition.
# ---------------------------------------------------------------------------


def test_abandon_without_a_precondition_is_refused_by_its_code(client, auth_headers):
    """`POST /plans/{id}/abandon` needs `If-Match`, and the URL hides where it goes.

    The verb sits last in the path, so a client keying validators by entity id
    finds nothing to attach and sends the write unconditional. The gear refuses
    it — correctly, since an unconditional lifecycle move would overwrite a
    concurrent editor — and the plan does not move.

    Written against a real 400 rather than a mocked one because the sentence the
    gear returns is the only thing that told a client author what was wrong.
    """
    plan_id, _, _ = _new_plan(client, auth_headers, with_phase=False)

    blind = client.post(
        f"/bss-pricing/v1/plans/{plan_id}/abandon",
        headers=auth_headers,
    )
    assert blind.status_code == 400, blind.text
    assert "If-Match" in blind.text

    # The plan is untouched: a refused precondition is not a partial write.
    still = client.get(f"/bss-pricing/v1/plans/{plan_id}", headers=auth_headers)
    assert still.status_code == 200, still.text
    assert still.json()["lifecycle_state"] == "draft"


def test_abandon_under_a_stale_precondition_is_refused_and_the_right_one_lands(
    client, auth_headers
):
    """The tag names a revision AND a version, and both are checked.

    `/plans/{planId}` names no revision, so a version alone would be applied to
    whichever revision the write resolved — which is why the validator is a pair.
    """
    plan_id, _, etag = _new_plan(client, auth_headers)

    stale = client.post(
        f"/bss-pricing/v1/plans/{plan_id}/abandon",
        headers={**auth_headers, "If-Match": '"0-99"'},
    )
    assert stale.status_code == 409, stale.text
    assert "STALE_VERSION" in _problem_codes(stale.json())

    landed = client.post(
        f"/bss-pricing/v1/plans/{plan_id}/abandon",
        headers={**auth_headers, "If-Match": etag},
    )
    assert landed.status_code == 200, landed.text
    assert landed.json()["lifecycle_state"] == "abandoned"


def test_cloning_a_draft_is_refused_because_a_clone_copies_a_published_revision(
    client, auth_headers
):
    """`POST /plans/{id}/clone` needs a PUBLISHED source, and says so by code.

    The route copies a **published** revision forward to author its successor —
    which is the ordinary way a catalog moves on — so a draft is not a source at
    all: `CLONE_SOURCE_NOT_FOUND`, 404.

    Written after a client offered Clone on every state and got this back. The
    refusal is now the assertion, because it is the behaviour that exists; a
    published source cannot be assembled here at all (no `CatalogVersion`
    registry gear is wired in this repository, so nothing publishes — see
    `test_pricing_seams`' module doc).
    """
    plan_id, _, _ = _new_plan(client, auth_headers, tier="silver")

    cloned = client.post(
        f"/bss-pricing/v1/plans/{plan_id}/clone",
        headers={**auth_headers, "Idempotency-Key": str(uuid.uuid4())},
    )
    assert cloned.status_code == 404, cloned.text
    assert "CLONE_SOURCE_NOT_FOUND" in cloned.text
    assert "published" in cloned.text


def test_a_row_naming_a_phase_the_plan_does_not_schedule_is_refused(
    client, auth_headers
):
    """A price row's `phase` axis has to name one of the plan's own phases.

    The other half of `_new_plan`'s phase step: a plan left as the create body
    describes it schedules nothing, so there is no phase a row can sit on and the
    row is refused rather than filed under a phase that does not exist.
    """
    plan_id, _, _ = _new_plan(client, auth_headers, with_phase=False)

    orphan = _add_row(
        client,
        auth_headers,
        plan_id,
        str(uuid.uuid4()),
        {"model_kind": "flat", "amount_minor": 1000},
    )
    assert orphan.status_code in (400, 409), orphan.text


# ---------------------------------------------------------------------------
# D-311: a rate is not an amount.
# ---------------------------------------------------------------------------


def test_a_per_unit_rate_round_trips_at_its_own_scale(client, auth_headers):
    """`unit_rate_nano_minor` comes back exactly, at 10⁻⁹ of a minor unit.

    The scale is the whole point of the column: `$0.0000166667` per GB-second is
    an ordinary metered rate and the amount column cannot express it. A client
    that reads or writes the rate one factor off still shows a plausible price,
    which is why the assertion is on the integer rather than on a rendering.
    """
    _declare_region(client, auth_headers)
    plan_id, phase_id, _ = _new_plan(client, auth_headers, tier="metered")

    # 0.023 major = 2.3 minor = 2_300_000_000 nano-minor.
    created = _add_row(
        client,
        auth_headers,
        plan_id,
        phase_id,
        {
            "model_kind": "per_unit",
            "unit_rate_nano_minor": 2_300_000_000,
            "meter": "cdn_gib",
            "billing_granularity": "whole_unit",
            "billing_timing": "arrears",
        },
        charge_kind="usage",
    )
    assert created.status_code == 201, created.text

    listed = client.get(
        f"/bss-pricing/v1/plans/{plan_id}/prices", headers=auth_headers
    )
    assert listed.status_code == 200, listed.text
    content = listed.json()["items"][0]["content"]
    assert content["unit_rate_nano_minor"] == 2_300_000_000
    # And it is NOT mirrored into the amount column, which would let a reader
    # take the same number twice at two different scales.
    assert content.get("amount_minor") is None


def test_a_rate_finer_than_a_cent_survives_the_wire(client, auth_headers):
    """The sub-cent case, asserted rather than assumed.

    `1_666_670` nano-minor is `$0.0000166667`. Anything that rounded to the minor
    unit on the way in or out would store it as zero and price the meter at
    nothing.
    """
    _declare_region(client, auth_headers)
    plan_id, phase_id, _ = _new_plan(client, auth_headers, tier="fine")

    created = _add_row(
        client,
        auth_headers,
        plan_id,
        phase_id,
        {
            "model_kind": "per_unit",
            "unit_rate_nano_minor": 1_666_670,
            "meter": "gib_seconds",
            "billing_granularity": "whole_unit",
            "billing_timing": "arrears",
        },
        charge_kind="usage",
    )
    assert created.status_code == 201, created.text
    assert created.json()["content"]["unit_rate_nano_minor"] == 1_666_670


def test_a_per_unit_row_priced_in_the_amount_column_is_stored_and_not_coerced(
    client, auth_headers
):
    """Save takes the row as authored; **placement is a publish rule**.

    `check_amount_placement` is per model kind and runs in both directions — a
    `per_unit` row carrying `amount_minor` is as wrong as a `flat` row carrying
    `unit_rate_nano_minor` — but it runs inside the publish rule set, not at
    save. So the row below is stored with the amount column filled and the rate
    column null, and it is publish that refuses it.

    This case first asserted a 400 at save, which was a guess about which door
    the rule sits behind. What it pins now is the part that is observable here
    and that a client actually has to know: **nothing on the write path stops a
    rate going into the amount column**, so a UI that lets a `per_unit` row be
    priced there has authored a row that cannot publish. The refusal itself is
    owned in-crate, where a publish can be driven (no `CatalogVersion` registry
    is wired in this repository).
    """
    _declare_region(client, auth_headers)
    plan_id, phase_id, _ = _new_plan(client, auth_headers, tier="misplaced")

    misplaced = _add_row(
        client,
        auth_headers,
        plan_id,
        phase_id,
        {
            "model_kind": "per_unit",
            "amount_minor": 2500,
            "meter": "cdn_gib",
            "billing_granularity": "whole_unit",
            "billing_timing": "arrears",
        },
        charge_kind="usage",
    )
    assert misplaced.status_code == 201, misplaced.text
    content = misplaced.json()["content"]
    # Stored where it was put, and the rate column left empty — so the row reads
    # as priced while the column its model kind prices from holds nothing.
    assert content["amount_minor"] == 2500
    assert content["unit_rate_nano_minor"] is None


# ---------------------------------------------------------------------------
# Bundles.
# ---------------------------------------------------------------------------


def _create_bundle(client, headers, plan_id, *, basis="own_price", itemization="itemize"):
    return client.post(
        "/bss-pricing/v1/bundles",
        headers={**headers, "Idempotency-Key": str(uuid.uuid4())},
        json={
            "plan_id": plan_id,
            "price_basis": basis,
            "invoice_itemization": itemization,
        },
    )


def test_a_bundle_is_created_read_and_listed(client, auth_headers):
    """Create → read the composition → find it on the page.

    The read is the part that mattered enough to build: the composition was
    reachable through no surface at all until D-310, which left it invisible to
    the approver of the always-material unit D-104 opens over it.
    """
    plan_id, _, _ = _new_plan(client, auth_headers, tier="bundle-host")

    created = _create_bundle(client, auth_headers, plan_id)
    assert created.status_code == 201, created.text
    bundle = created.json()
    bundle_id = bundle["bundle_id"]
    assert bundle["plan_id"] == plan_id
    assert bundle["price_basis"] == "own_price"
    assert bundle["invoice_itemization"] == "itemize"

    read = client.get(f"/bss-pricing/v1/bundles/{bundle_id}", headers=auth_headers)
    assert read.status_code == 200, read.text
    composition = read.json()
    # A fresh bundle is a declaration with nothing in it yet — and the empty sets
    # are present rather than absent, so a reader never has to guess.
    assert composition["components"] == []
    assert composition["rev_share"] == []
    assert composition["plan_revision"] == 0

    listed = client.get("/bss-pricing/v1/bundles", headers=auth_headers)
    assert listed.status_code == 200, listed.text
    page = listed.json()
    assert bundle_id in [row["bundle_id"] for row in page["items"]]
    # The list carries the declaration only: no component or rev-share member,
    # because a bundle's composition is three further queries each.
    row = next(r for r in page["items"] if r["bundle_id"] == bundle_id)
    assert set(row) == {
        "bundle_id",
        "plan_id",
        "price_basis",
        "invoice_itemization",
    }


def test_a_price_basis_outside_the_two_the_gear_knows_is_refused(client, auth_headers):
    """`price_basis` is closed: `sum_of_parts` or `own_price`.

    Asserted because the published document types it as a bare string, so the
    only place the two legal tokens are stated is the refusal.
    """
    plan_id, _, _ = _new_plan(client, auth_headers, tier="bad-basis")

    refused = _create_bundle(client, auth_headers, plan_id, basis="bundle_price")
    assert refused.status_code == 400, refused.text
    assert "sum_of_parts" in refused.text and "own_price" in refused.text


def test_a_plan_carries_at_most_one_bundle(client, auth_headers):
    """The second bundle on one plan is a conflict, not a second row.

    A client that offers every plan in a create dialog is offering a refusal; the
    pricing MFE narrows its pick-list to plans without one because of this.
    """
    plan_id, _, _ = _new_plan(client, auth_headers, tier="one-bundle")

    first = _create_bundle(client, auth_headers, plan_id)
    assert first.status_code == 201, first.text

    second = _create_bundle(client, auth_headers, plan_id)
    assert second.status_code == 409, second.text


def test_a_composition_patch_without_a_precondition_is_refused(client, auth_headers):
    """`PATCH /bundles/{id}` asserts the PLAN revision's tag, and demands one.

    The composition is revision-scoped, so its concurrency story is the plan
    revision's — not a tag of the bundle's own. The published document declares
    no request body for this operation and no precondition either; both are
    required.
    """
    plan_id, _, plan_tag = _new_plan(client, auth_headers, tier="patch-me")
    bundle_id = _create_bundle(client, auth_headers, plan_id).json()["bundle_id"]

    blind = client.patch(
        f"/bss-pricing/v1/bundles/{bundle_id}",
        headers=auth_headers,
        json={"plan_revision": 0, "components": [], "rev_share": []},
    )
    assert blind.status_code == 400, blind.text
    assert "If-Match" in blind.text


def test_a_composition_lands_and_reads_back_whole(client, auth_headers):
    """Components and rev-share groups, written wholesale and read back.

    The rev-share group is asserted in full because publish reconciles it against
    100 % (D-07) and normalizes the residual onto the absorber — so what a reader
    is shown before publish has to be what was authored, not what publish will
    make of it.
    """
    host_id, _, host_tag = _new_plan(client, auth_headers, tier="composed")
    component_id, _, _ = _new_plan(client, auth_headers, tier="component")
    bundle_id = _create_bundle(client, auth_headers, host_id).json()["bundle_id"]

    sku_id = str(uuid.uuid4())
    vendor_sku_id = str(uuid.uuid4())
    patched = client.patch(
        f"/bss-pricing/v1/bundles/{bundle_id}",
        headers={**auth_headers, "If-Match": host_tag},
        json={
            "plan_revision": 0,
            "components": [
                {
                    "component_plan_id": component_id,
                    "included_sku_id": sku_id,
                    "min_qty": 1,
                    "max_qty": 10,
                }
            ],
            "rev_share": [
                {
                    "vendor_sku_id": vendor_sku_id,
                    "platform_cut_bp": 2000,
                    "residual_absorber_party": "reseller-emea",
                    "parties": [{"party": "vendor-acme", "share_bp": 6000}],
                }
            ],
        },
    )
    assert patched.status_code == 200, patched.text
    assert patched.json()["bundle_id"] == bundle_id

    read = client.get(f"/bss-pricing/v1/bundles/{bundle_id}", headers=auth_headers)
    assert read.status_code == 200, read.text
    composition = read.json()
    assert [c["component_plan_id"] for c in composition["components"]] == [component_id]
    assert composition["components"][0]["min_qty"] == 1
    assert composition["components"][0]["max_qty"] == 10

    group = composition["rev_share"][0]
    assert group["vendor_sku_id"] == vendor_sku_id
    assert group["platform_cut_bp"] == 2000
    assert group["residual_absorber_party"] == "reseller-emea"
    assert group["parties"] == [{"party": "vendor-acme", "share_bp": 6000}]
    # 20 % + 60 % declared, so 20 % is the residual the absorber takes at publish.
    declared = group["platform_cut_bp"] + sum(p["share_bp"] for p in group["parties"])
    assert declared == 8000


def test_publishing_a_bundle_needs_a_body_the_document_does_not_declare(
    client, auth_headers
):
    """`POST /bundles/{id}/publish` reads `{plan_revision, markets}`.

    The published `openapi.json` declares no request body for this operation at
    all, so a caller that trusts the document sends nothing — and is refused for
    a missing field rather than for anything about the composition. That gap cost
    a client author a debugging round; this is the assertion that makes it cost a
    test instead.
    """
    plan_id, _, _ = _new_plan(client, auth_headers, tier="publish-me")
    bundle_id = _create_bundle(client, auth_headers, plan_id).json()["bundle_id"]

    empty = client.post(
        f"/bss-pricing/v1/bundles/{bundle_id}/publish",
        headers={**auth_headers, "Idempotency-Key": str(uuid.uuid4())},
        json={},
    )
    assert empty.status_code == 400, empty.text
    # A missing field, not a rule failure: the two read very differently to a
    # caller trying to work out what it did wrong.
    assert "plan_revision" in empty.text or "markets" in empty.text


def test_a_bundle_publish_reports_every_violation_in_one_pass(client, auth_headers):
    """The rule set answers whole, which is what makes the refusal actionable.

    A component plan that has not published blocks the composition; so does a
    market with no coverage. A caller shown only the first would fix one and come
    back for the next, which is why the response carries the list and why a client
    that renders only the status line throws the answer away.
    """
    host_id, _, host_tag = _new_plan(client, auth_headers, tier="unpublishable")
    component_id, _, _ = _new_plan(client, auth_headers, tier="draft-component")
    bundle_id = _create_bundle(client, auth_headers, host_id).json()["bundle_id"]

    client.patch(
        f"/bss-pricing/v1/bundles/{bundle_id}",
        headers={**auth_headers, "If-Match": host_tag},
        json={
            "plan_revision": 0,
            "components": [
                {
                    "component_plan_id": component_id,
                    "included_sku_id": str(uuid.uuid4()),
                    "min_qty": 1,
                    "max_qty": None,
                }
            ],
            "rev_share": [],
        },
    )

    refused = client.post(
        f"/bss-pricing/v1/bundles/{bundle_id}/publish",
        headers={**auth_headers, "Idempotency-Key": str(uuid.uuid4())},
        json={"plan_revision": 0, "markets": [{"currency": "USD", "region": "EU"}]},
    )
    assert refused.status_code == 400, refused.text
    codes = _problem_codes(refused.json())
    assert "COMPONENT_UNPUBLISHED" in codes, refused.text
    # The list, not just a status line.
    assert _violations(refused.json()), refused.text


# ---------------------------------------------------------------------------
# Price overlays.
# ---------------------------------------------------------------------------


def _overlay_body(**over):
    body = {
        "scope_class": "customer_group",
        "scope_value": "gold-partners",
        "precedence": 100,
        "tax_basis": "exclusive",
        "disclosure": "restricted",
        "target_plan_ids": [],
        "lines": [
            {
                "adjustment_kind": "discount",
                "magnitude_kind": "percent_bp",
                "adjustment_value": 1250,
                "amounts": [],
            }
        ],
    }
    body.update(over)
    return body


def _create_overlay(client, headers, **over):
    return client.post(
        "/bss-pricing/v1/price-overlays",
        headers={**headers, "Idempotency-Key": str(uuid.uuid4())},
        json=_overlay_body(**over),
    )


def test_an_overlay_is_created_and_the_list_uses_its_own_envelope(
    client, auth_headers
):
    """`GET /price-overlays` answers `{overlays, page_info}`, not `{items}`.

    Every other list on this gear uses `items`. A client that assumes the shared
    envelope reads `undefined` and renders an empty stack — with no error, because
    nothing failed. The envelope is asserted by name for that reason.
    """
    created = _create_overlay(client, auth_headers)
    assert created.status_code == 201, created.text
    # The create answers an ACCEPTED view — the id and the revision it opened —
    # and not the overlay. A client that types this response as the full record
    # is describing something the route never sends.
    accepted = created.json()
    assert set(accepted) == {"price_overlay_id", "revision"}, accepted
    overlay_id = accepted["price_overlay_id"]

    listed = client.get("/bss-pricing/v1/price-overlays", headers=auth_headers)
    assert listed.status_code == 200, listed.text
    page = listed.json()
    assert "overlays" in page, f"the list envelope changed shape: {sorted(page)}"
    assert "items" not in page
    row = next(r for r in page["overlays"] if r["price_overlay_id"] == overlay_id)
    assert row["scope_class"] == "customer_group"
    assert row["scope_value"] == "gold-partners"
    assert row["precedence"] == 100
    assert row["lifecycle_state"] == "draft"


def test_an_overlay_carries_its_percentage_as_basis_points(client, auth_headers):
    """`12.5 %` travels as `1250`, and comes back as `1250`.

    The unit is basis points so the comparison stays integer; a client that sent
    the percentage itself would author a hundredth of the discount it meant, and
    the overlay would look authored.
    """
    created = _create_overlay(client, auth_headers, precedence=101)
    assert created.status_code == 201, created.text
    overlay_id = created.json()["price_overlay_id"]

    # Read back off the list, since the create answers an id and a revision only.
    listed = client.get("/bss-pricing/v1/price-overlays", headers=auth_headers)
    row = next(
        r for r in listed.json()["overlays"] if r["price_overlay_id"] == overlay_id
    )
    line = row["lines"][0]
    assert line["magnitude_kind"] == "percent_bp"
    assert line["adjustment_value"] == 1250


def test_a_global_overlay_may_not_also_name_a_scope_value(client, auth_headers):
    """The class/value pairing is unrepresentable, so a mismatch is refused.

    `global` means every payer; a value alongside it would narrow the same
    overlay two ways at once. The gear refuses rather than normalizing, which is
    why a client hides the field instead of sending it empty.
    """
    refused = _create_overlay(
        client, auth_headers, scope_class="global", scope_value="gold-partners"
    )
    assert refused.status_code == 400, refused.text


def test_an_overlay_with_no_line_is_accepted_and_then_refused_at_submit(
    client, auth_headers
):
    """D-42's ≥1-line rule lives at **submit**, and this pins which door it is.

    `overlay_rules::validate` — the whole aggregate set — runs in the submit
    handler (`inst-pl-validate`), not in the create. So a zero-line overlay is
    stored as a draft and refused the moment it is raised for approval, by
    `OVERLAY_HAS_NO_LINE`.

    Asserted in both halves deliberately. This case first claimed the create
    refuses it, which was a guess about where the rule sits — and a guess of that
    kind is how a client ends up validating on the wrong side and reporting an
    error the gear never raised.
    """
    created = _create_overlay(client, auth_headers, lines=[], precedence=102)
    assert created.status_code == 201, created.text
    overlay_id = created.json()["price_overlay_id"]

    submitted = client.post(
        f"/bss-pricing/v1/price-overlays/{overlay_id}/submit",
        headers={**auth_headers, "Idempotency-Key": str(uuid.uuid4())},
        json={"revision": 0},
    )
    assert submitted.status_code in (400, 422), submitted.text
    assert "OVERLAY_HAS_NO_LINE" in _problem_codes(submitted.json()), submitted.text


def test_an_undeclared_tax_basis_is_refused_by_its_code(client, auth_headers):
    """L5: the basis MUST be declared, and `TAX_BASIS_UNDECLARED` is what says so.

    Modelled as optional on the wire precisely so this code is reachable — a
    required field would be refused by the deserializer with a message the design
    set does not own.
    """
    body = _overlay_body(precedence=103)
    body.pop("tax_basis")
    refused = client.post(
        "/bss-pricing/v1/price-overlays",
        headers={**auth_headers, "Idempotency-Key": str(uuid.uuid4())},
        json=body,
    )
    assert refused.status_code == 400, refused.text
    assert "TAX_BASIS_UNDECLARED" in _problem_codes(refused.json()), refused.text


def test_a_customer_group_overlay_is_validated_against_its_declared_universe(
    client, auth_headers
):
    """`SCOPE_VALUE_UNKNOWN` now discriminates, where it used to refuse everything.

    This asserted that **every** `customer_group` overlay is refused at submit,
    because `taxonomy_declares` answered `false` unconditionally for the class.
    D-223 chose that while the class had no value universe at all, and said so in
    its own words — *"reversible in one line when the membership half lands"*. It
    landed: `m20260802_000066` creates `pricing_customer_group_taxonomy`,
    `TaxonomyRepo::replace_customer_groups` writes it, and
    `api::rest::customer_groups` mounts the `GET`/`PUT` pair this test's own
    :func:`_declare_customer_group` already drives. Left as it was, the arm refused an
    overlay against a universe that **exists**, which is not a fail-closed reading
    of anything — only a stale one.

    So the subject moves from "refused" to "discriminates", and both directions are
    asserted over the same route in the same test, because the value of the change
    is precisely that they now differ:

    * a **declared, active** value is accepted and raises an approval unit;
    * an **undeclared** one is still refused by `SCOPE_VALUE_UNKNOWN`, naming the
      taxonomy it is not in.

    The second half is the one that would have been lost by simply flipping the
    expectation, and it is the half `overlay_repo` calls unchanged: an undeclared
    value, and a `retired` one, answer `false` exactly as on the four sibling
    taxonomies.
    """
    _declare_customer_group(client, auth_headers)

    # Declared: accepted, and it raises a unit rather than publishing (D-10 makes
    # any overlay mutation always-material).
    created = _create_overlay(client, auth_headers, precedence=106)
    overlay_id = created.json()["price_overlay_id"]
    accepted = client.post(
        f"/bss-pricing/v1/price-overlays/{overlay_id}/submit",
        headers={**auth_headers, "Idempotency-Key": str(uuid.uuid4())},
        json={"revision": 0},
    )
    assert accepted.status_code == 202, accepted.text
    body = accepted.json()
    assert body["outcome"] == "submitted_for_approval", body
    assert body["approval"]["state"] == "submitted", body
    assert body["approval"]["materiality"]["trigger"] == "priceOverlayMutation", body

    # Undeclared: still refused, and the refusal still names where to look.
    stranger = _create_overlay(
        client, auth_headers, precedence=107, scope_value="never-declared-group"
    )
    refused = client.post(
        f"/bss-pricing/v1/price-overlays/{stranger.json()['price_overlay_id']}/submit",
        headers={**auth_headers, "Idempotency-Key": str(uuid.uuid4())},
        json={"revision": 0},
    )
    assert refused.status_code == 400, refused.text
    assert "SCOPE_VALUE_UNKNOWN" in _problem_codes(refused.json()), refused.text
    assert "pricing_customer_group_taxonomy" in refused.text


def test_submitting_an_overlay_raises_a_unit_rather_than_publishing_it(
    client, auth_headers
):
    """`POST /price-overlays/{id}/submit` answers 202, and the overlay stays put.

    An overlay changes what a payer is charged, so it goes through the same
    approval plane as a plan revision. The 202 is a unit being raised; a client
    that read it as "published" would tell an author the discount is live.

    Scoped `global` rather than `customer_group`: the classless scope consults no
    taxonomy and so has nothing undeclared, which is what makes it the one class
    that can reach the approval plane in this strand. See the case above.
    """
    created = _create_overlay(
        client, auth_headers, scope_class="global", scope_value=None, precedence=104
    )
    assert created.status_code == 201, created.text
    overlay_id = created.json()["price_overlay_id"]

    # The body carries the **revision** being submitted, and it is required:
    # `SubmitOverlayRequest { revision }`. An absent body is refused for the body,
    # and `{}` is refused for the field — so a client that sends neither never
    # submits anything, and reads two 400s that say nothing about its overlay.
    submitted = client.post(
        f"/bss-pricing/v1/price-overlays/{overlay_id}/submit",
        headers={**auth_headers, "Idempotency-Key": str(uuid.uuid4())},
        json={"revision": 0},
    )
    assert submitted.status_code == 202, submitted.text

    listed = client.get("/bss-pricing/v1/price-overlays", headers=auth_headers)
    state = next(
        row["lifecycle_state"]
        for row in listed.json()["overlays"]
        if row["price_overlay_id"] == overlay_id
    )
    assert state != "published", (
        "submit put the overlay live; it is supposed to raise an approval unit "
        f"and leave the overlay where it was: {state}"
    )


def test_submit_without_a_json_body_is_refused_before_any_rule_runs(
    client, auth_headers
):
    """An absent body is a 400 about the body, not about the overlay.

    The handler parses a `SubmitOverlayRequest`, and the route's own message says
    what to send: `{}` for an empty one. Pinned because a client that omits it
    reads the refusal as a rule failure and looks for a problem with the overlay
    that is not there.
    """
    created = _create_overlay(client, auth_headers, precedence=105)
    overlay_id = created.json()["price_overlay_id"]

    empty = client.post(
        f"/bss-pricing/v1/price-overlays/{overlay_id}/submit",
        headers={**auth_headers, "Idempotency-Key": str(uuid.uuid4())},
    )
    assert empty.status_code == 400, empty.text
    assert "body is empty" in empty.text

    # And `{}` is refused too, for the field rather than the body: the revision
    # is what the submit is *of*.
    without_revision = client.post(
        f"/bss-pricing/v1/price-overlays/{overlay_id}/submit",
        headers={**auth_headers, "Idempotency-Key": str(uuid.uuid4())},
        json={},
    )
    assert without_revision.status_code == 400, without_revision.text
    assert "revision" in without_revision.text


@pytest.mark.parametrize(
    "path",
    [
        "/bss-pricing/v1/bundles",
        "/bss-pricing/v1/price-overlays",
    ],
)
def test_the_composition_lists_are_authenticated(client, path):
    """Both new lists sit behind the PEP, like every other route on the gear."""
    unauthenticated = client.get(path)
    assert unauthenticated.status_code == 401, unauthenticated.text
