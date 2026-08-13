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
`docker ps -aq`, which includes RUNNING containers — not just dead ones. If
these self-tests used a sidecar's shared `LABEL`, planting an orphan under that
label (or `start()`'s own reap-before-run) would delete the container the real
`test_env` session fixture has pooled connections against. So every sidecar and
label reference below uses a distinct `*-selftest` label instead of the shared
one — do not "simplify" this back to `<Sidecar>.LABEL`.
"""
import shutil
import socket
import subprocess

import pytest

from lib.sidecars import ClickHouseSidecar, TimescaleDbSidecar

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
# a database to accept connections takes far longer, so both tests below opt
# out explicitly. The package conftest (Task 4) leaves an existing timeout
# marker alone, so these values survive.


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
    remaining = subprocess.run(
        ["docker", "ps", "-aq", "--filter", f"id={container_id}"],
        capture_output=True, text=True, timeout=30, check=True,
    ).stdout.strip()
    assert remaining == "", f"container {container_id} survived stop()"


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
        sidecar_cls.reap_orphans(selftest_label)
        remaining = subprocess.run(
            ["docker", "ps", "-aq", "--filter", f"id={orphan}"],
            capture_output=True, text=True, timeout=30, check=True,
        ).stdout.strip()
        assert remaining == "", "reap_orphans() left a labelled container behind"
    finally:
        subprocess.run(["docker", "rm", "-f", orphan],
                       capture_output=True, timeout=60, check=False)
