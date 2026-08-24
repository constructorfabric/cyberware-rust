"""Contract test for the database sidecars — verifies the Docker lifecycle itself.

Not an E2E test: no server, no gear. It exists because every other test in this
directory depends on these classes behaving, and debugging a container problem
through a failed server boot is far slower than failing here.

It lives in this package rather than next to lib/sidecars.py so that it
inherits this package's conftest skip gate (Task 4's `_require_dedicated_binary`
autouse fixture): the shared `make e2e-local` run DOES collect this file (it's
part of `testpaths = .`), but every test in it is skipped unless `E2E_BINARY`
is set, same as the rest of this package.

Both sidecars are covered, but only the one matching `UC_E2E_BACKEND` runs in
any given invocation. The two backends run as separate pytest sessions
(see conftest), so without that gate the TimescaleDB image would be pulled and
booted during the ClickHouse session and vice versa — pure cost for a class
that session is not using.

Isolation note: `reap_orphans()` matches on Docker label alone, via
`docker ps -aq`, which includes RUNNING containers — not just dead ones. A real
session's container carries a per-run label (class prefix plus `RUN_ID`), so it
is already out of reach of anything here; the `*-selftest` labels below add a
second layer, keeping this file's planted orphans in a namespace that is
deterministic (no `RUN_ID`) and disjoint from any session's. Do not "simplify"
these back to `<Sidecar>.LABEL`: that constant is a prefix, so a container
would match it only by accident of a shared prefix, and the reap-before-run in
`start()` would then be aimed at whatever else shares it.
"""
import shutil
import socket
import subprocess

import pytest

from lib.sidecars import (
    LABEL_KEY,
    REAP_MIN_AGE_SECS,
    RUN_ID,
    ClickHouseSidecar,
    TimescaleDbSidecar,
)

from .conftest import BACKEND

# One per sidecar, each distinct from that class's shared LABEL and from each
# other. See the isolation note above.
SIDECARS = [
    pytest.param(
        TimescaleDbSidecar,
        "cf-gears-e2e=usage-collector-selftest",
        id="timescaledb",
        marks=pytest.mark.skipif(
            BACKEND != "timescaledb",
            reason=f"UC_E2E_BACKEND={BACKEND}: not this session's sidecar",
        ),
    ),
    pytest.param(
        ClickHouseSidecar,
        "cf-gears-e2e=usage-collector-ch-selftest",
        id="clickhouse",
        marks=pytest.mark.skipif(
            BACKEND != "clickhouse",
            reason=f"UC_E2E_BACKEND={BACKEND}: not this session's sidecar",
        ),
    ),
]


def _exists(container_id: str) -> bool:
    """True while Docker still knows `container_id` (running or exited)."""
    return subprocess.run(
        ["docker", "ps", "-aq", "--filter", f"id={container_id}"],
        capture_output=True, text=True, timeout=30, check=True,
    ).stdout.strip() != ""


def _docker_available() -> bool:
    if shutil.which("docker") is None:
        return False
    return subprocess.run(
        ["docker", "info"], capture_output=True, timeout=30, check=False
    ).returncode == 0


pytestmark = pytest.mark.skipif(
    not _docker_available(), reason="Docker is not available"
)

# pytest.ini imposes a 10s per-test hard kill. Pulling an image and waiting for
# a database to accept connections takes far longer, so every test below that
# touches Docker opts out explicitly. The package conftest (Task 4) leaves an
# existing timeout marker alone, so these values survive. The label test needs
# no marker: it constructs sidecars and touches no container.


@pytest.mark.timeout(600)
@pytest.mark.parametrize("sidecar_cls,selftest_label", SIDECARS)
def test_sidecar_starts_exposes_a_reachable_port_and_cleans_up(
    sidecar_cls, selftest_label
):
    sidecar = sidecar_cls(label=selftest_label)
    # Ties the class back to the key conftest selected it under: _patch_config
    # finds the sidecar by `name`, so a mismatch would strand the DSN
    # placeholder unsubstituted.
    assert sidecar.name == BACKEND
    assert sidecar.port is None, "port must be unknown before start()"

    container_port = int(sidecar_cls.CONTAINER_PORT.split("/")[0])

    sidecar.start()
    try:
        assert sidecar.port is not None
        assert sidecar.port != container_port, (
            "must use a mapped port, not the container's own"
        )
        with socket.create_connection(("127.0.0.1", sidecar.port), timeout=5):
            pass
        assert sidecar.dsn_port == str(sidecar.port)
    finally:
        container_id = sidecar.container_id
        sidecar.stop()

    assert container_id is not None
    assert not _exists(container_id), f"container {container_id} survived stop()"


@pytest.mark.parametrize("sidecar_cls,selftest_label", SIDECARS)
def test_default_label_is_per_run_not_the_class_constant(
    sidecar_cls, selftest_label
):
    """The default label must name THIS process, not the class.

    `start()` reaps its own label before running, and that reap removes running
    containers, so a label shared by every run of a class means one session's
    startup deletes a concurrent session's live database.
    """
    default_label = sidecar_cls().label
    assert default_label != sidecar_cls.LABEL, (
        "the default label must not be the shared class constant"
    )
    assert default_label.startswith(f"{sidecar_cls.LABEL}-")
    assert default_label.endswith(RUN_ID)
    assert default_label.startswith(f"{LABEL_KEY}="), (
        "the family key must survive, or reap_stale cannot find the container"
    )
    # An explicit label is still taken verbatim — this file depends on it.
    assert sidecar_cls(label=selftest_label).label == selftest_label


@pytest.mark.timeout(600)
@pytest.mark.parametrize("sidecar_cls,selftest_label", SIDECARS)
def test_reap_stale_spares_a_concurrent_run_and_removes_a_leaked_one(
    sidecar_cls, selftest_label
):
    """The age gate is what separates "leaked" from "someone else is using it".

    A fresh container under another run's label value stands in for a
    concurrent session: the default sweep must not touch it. Dropping the age
    floor must then select it, which is what proves it was the age — not a
    failure to match the family key — that spared it.

    The age-0 case asserts the DECISION via `stale_ids` and does not execute
    the sweep: `reap_stale(min_age_secs=0)` removes every labelled container on
    the host, so running it here would delete a live session's database — the
    exact bug this test exists to pin.
    """
    sidecar_cls.pull()  # see the sibling test below for why this is explicit

    other_run_label = f"{selftest_label}-otherrun"
    orphan = subprocess.run(
        ["docker", "run", "-d", "--label", other_run_label,
         "--entrypoint", "sleep", sidecar_cls.IMAGE, "300"],
        capture_output=True, text=True, timeout=120, check=True,
    ).stdout.strip()
    try:
        assert orphan not in sidecar_cls.stale_ids(), (
            f"a container younger than {REAP_MIN_AGE_SECS}s was selected for "
            "reaping — a concurrent session would have lost its database"
        )
        sidecar_cls.reap_stale()
        assert _exists(orphan), "reap_stale() removed a fresh foreign container"

        assert orphan in sidecar_cls.stale_ids(min_age_secs=0), (
            "with no age floor the container must be selected, or the default "
            "sweep spared it for the wrong reason"
        )
    finally:
        subprocess.run(["docker", "rm", "-f", orphan],
                       capture_output=True, timeout=60, check=False)


@pytest.mark.timeout(600)
@pytest.mark.parametrize("sidecar_cls,selftest_label", SIDECARS)
def test_reap_stale_never_removes_its_own_run(sidecar_cls, selftest_label):
    """`own_label` is skipped whatever its age says.

    `start()` passes it so that a skewed daemon clock — or a threshold lowered
    in config — can never make a session reap the container it just started.
    Asserted through `stale_ids` for the same reason as the sibling test: at
    `min_age_secs=0` the sweep itself is host-wide.
    """
    sidecar_cls.pull()

    own_label = f"{selftest_label}-ownrun"
    mine = subprocess.run(
        ["docker", "run", "-d", "--label", own_label,
         "--entrypoint", "sleep", sidecar_cls.IMAGE, "300"],
        capture_output=True, text=True, timeout=120, check=True,
    ).stdout.strip()
    # A second backend of the SAME run: different class prefix, same RUN_ID
    # suffix. `own_label` cannot spare it, so only the run identity can.
    sibling_label = f"{selftest_label}-sibling-{RUN_ID}"
    sibling = subprocess.run(
        ["docker", "run", "-d", "--label", sibling_label,
         "--entrypoint", "sleep", sidecar_cls.IMAGE, "300"],
        capture_output=True, text=True, timeout=120, check=True,
    ).stdout.strip()
    try:
        selected = sidecar_cls.stale_ids(min_age_secs=0, own_label=own_label)
        assert mine not in selected, "own run's container selected for reaping"
        assert sibling not in selected, (
            "a container from this run under another class prefix was selected "
            "— a second sidecar's start() would have reaped it mid-session"
        )
        # The full `key=value` form must work: callers hold labels in that
        # shape, while `docker inspect` reports bare values.
        assert own_label.startswith(f"{LABEL_KEY}=")
    finally:
        subprocess.run(["docker", "rm", "-f", mine, sibling],
                       capture_output=True, timeout=60, check=False)


@pytest.mark.timeout(600)
@pytest.mark.parametrize("sidecar_cls,selftest_label", SIDECARS)
def test_reap_removes_orphans_from_a_previous_run(sidecar_cls, selftest_label):
    # `docker run` below carries a 120s timeout, which an implicit pull would
    # overrun on a cold cache — and a TimeoutExpired there fires BEFORE the
    # `try`, so the orphan would leak instead of being removed. The sibling
    # test above pulls via start(), but this test must stand alone under
    # `-k test_reap`. See _DockerSidecar.pull.
    sidecar_cls.pull()

    orphan = subprocess.run(
        ["docker", "run", "-d", "--label", selftest_label,
         "--entrypoint", "sleep", sidecar_cls.IMAGE, "300"],
        capture_output=True, text=True, timeout=120, check=True,
    ).stdout.strip()
    try:
        # No age floor by default: an exact label match is proof of ownership,
        # so a fresh orphan planted under it must go immediately.
        sidecar_cls.reap_orphans(selftest_label)
        assert not _exists(orphan), (
            "reap_orphans() left a labelled container behind"
        )
    finally:
        subprocess.run(["docker", "rm", "-f", orphan],
                       capture_output=True, timeout=60, check=False)
