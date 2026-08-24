"""Container sidecars for the E2E orchestrator.

Each class here implements ``lib.orchestrator.SidecarProtocol`` (``name``,
``port``, ``start()``, ``stop()``) so it can be dropped into
``GearTestEnv.sidecars``. The orchestrator starts sidecars before it renders
the config, so ``config_patch`` can read the mapped port.

Deliberately hand-rolled rather than using testcontainers-python: this suite's
requirements.txt is minimal and Snyk-pinned, and testcontainers-rs 0.27 (used
by the Rust plugin tests) ships no ryuk either, so the library would buy no
cleanup guarantee the repo already relies on. Orphans are handled by the label
reapers below.

Labels are two-part, and the split matters. Every container carries the same
KEY (``LABEL_KEY``) so a sweep can find sidecars from any run, while the VALUE
ends in ``RUN_ID`` so it names one pytest process. Cleanup is what forces this:
removal is by label and ``docker ps -aq`` lists running containers, so a single
fixed label per class would make one session's startup delete a concurrent
session's live database. ``reap_orphans`` therefore addresses one exact label
(this run's, or a test's own), and cross-run cleanup goes through the age-gated
``reap_stale``.

``_DockerSidecar`` owns the lifecycle every container shares; a subclass
supplies its image, its environment, and its own readiness probe.
"""

from __future__ import annotations

import atexit
import os
import shutil
import subprocess
import time
import uuid
from datetime import datetime, timezone

import pytest

DOCKER_TIMEOUT = 120
# Image pulls get their own, far more generous budget. See _DockerSidecar.pull.
PULL_TIMEOUT = 600
READY_TIMEOUT = 60
READY_POLL_INTERVAL = 0.5
# Consecutive successful query probes required before a container is called
# ready. See _DockerSidecar._wait_ready for why one success is not enough.
REQUIRED_CONSECUTIVE_OK = 3

# The TimescaleDB pin, mirrored from libs/test-containers/src/lib.rs (TIMESCALEDB_IMAGE,
# TIMESCALEDB_TAG, ENV_TIMESCALEDB_TAG). Split into repo + tag rather than one composed
# literal so the environment override below can reach this lane too; the Rust test
# `e2e_sidecar_pins_the_same_timescaledb_image` fails the build if any of the three
# drifts from its constant.
TIMESCALEDB_IMAGE = "timescale/timescaledb"
TIMESCALEDB_TAG = "2.29.2-pg18"
ENV_TIMESCALEDB_TAG = "GEARS_TEST_TIMESCALEDB_TAG"


def timescaledb_tag() -> str:
    """Effective TimescaleDB tag, honoring GEARS_TEST_TIMESCALEDB_TAG.

    Unset *or empty* means "use the pinned constant", matching
    `test_containers::timescaledb_tag()` on the Rust side. Without this, a CI
    version matrix would move the Rust plugin tests while leaving E2E on the
    default -- migrations validated against one PostgreSQL major, E2E run
    against another, with nothing in the diff to show it.
    """
    return os.environ.get(ENV_TIMESCALEDB_TAG, "").strip() or TIMESCALEDB_TAG


# Docker label KEY every sidecar here shares. The VALUE identifies the run that
# owns the container: a class constant prefix plus RUN_ID. Splitting the two
# lets `reap_stale` find every sidecar container ever started (filter on the
# key) while `reap_orphans` still addresses one exact namespace.
LABEL_KEY = "cf-gears-e2e"

# One identity per pytest process, so two sessions on the same host never share
# a label value. Overridable so CI can pin a run identity across processes (or
# reuse one deliberately, e.g. to let a retry reap its own leftovers).
RUN_ID = (
    os.environ.get("CF_GEARS_E2E_RUN_ID")
    or f"{os.getpid()}-{uuid.uuid4().hex[:8]}"
)

# How old a foreign container must be before `reap_stale` treats it as leaked.
# There is no way to ask Docker whether the process that started a container is
# still alive, so age is the proxy: this MUST exceed the longest plausible
# session (cold image pull plus a full suite) or a new run would delete a live
# concurrent one — the very bug the per-run label exists to prevent.
REAP_MIN_AGE_SECS = int(os.environ.get("CF_GEARS_E2E_REAP_MIN_AGE_SECS", "3600"))


class DockerUnavailable(RuntimeError):
    """Docker CLI missing, or the daemon is not reachable."""


def _run(args: list[str], *, timeout: int = DOCKER_TIMEOUT) -> str:
    proc = subprocess.run(
        args, capture_output=True, text=True, timeout=timeout, check=False,
    )
    if proc.returncode != 0:
        raise RuntimeError(
            f"command failed: {' '.join(args)}\n"
            f"exit={proc.returncode}\nstdout={proc.stdout}\nstderr={proc.stderr}"
        )
    return proc.stdout.strip()


def require_docker() -> None:
    """Raise DockerUnavailable unless a reachable Docker daemon is present."""
    if shutil.which("docker") is None:
        raise DockerUnavailable("the `docker` CLI is not on PATH")
    proc = subprocess.run(
        ["docker", "info"], capture_output=True, text=True, timeout=30, check=False,
    )
    if proc.returncode != 0:
        raise DockerUnavailable(f"docker daemon unreachable: {proc.stderr.strip()}")


def skip_without_docker() -> None:
    """pytest.skip (module-level) unless Docker is usable."""
    try:
        require_docker()
    except DockerUnavailable as exc:
        pytest.skip(f"Docker required for this suite: {exc}", allow_module_level=True)


class _DockerSidecar:
    """A throwaway container with a dynamically mapped host port.

    Subclasses set `IMAGE`, `LABEL`, `CONTAINER_PORT` and `name`, then
    implement `_env_args` (the container's environment) and `_probe` (one
    readiness attempt). Everything else — reap, pull, run, port lookup, the
    consecutive-OK readiness wait, and teardown — is shared.
    """

    # Subclass contract.
    IMAGE = ""
    # `key=value` prefix, NOT the label a container ends up with: the instance
    # appends RUN_ID (see __init__). Every subclass MUST use LABEL_KEY as the
    # key, since that is what `reap_stale` sweeps on.
    LABEL = ""
    # The container-side port to map, in `docker port` syntax, e.g. "5432/tcp".
    CONTAINER_PORT = ""
    name = ""

    def __init__(self, label: str | None = None) -> None:
        # Default to a per-RUN label, not the bare class constant: `start()`
        # reaps before it runs, and the reap filter matches RUNNING containers,
        # so two sessions sharing one label value delete each other's live
        # database mid-run. An explicit `label` is taken verbatim, which is how
        # the self-tests in test_sidecar_contract.py get a deterministic
        # namespace they can plant and reap orphans in.
        self.label = label or f"{self.LABEL}-{RUN_ID}"
        self.port: int | None = None
        self.container_id: str | None = None

    @property
    def dsn_port(self) -> str:
        """Mapped host port as a string, for config substitution."""
        if self.port is None:
            raise RuntimeError(f"{type(self).__name__}.start() has not run")
        return str(self.port)

    @staticmethod
    def _ids_labelled(label_filter: str) -> list[str]:
        """Container ids matching a `docker ps` label filter.

        `label_filter` is either `key=value` (one exact namespace) or a bare
        `key`, which Docker matches against any value.
        """
        ids = _run(["docker", "ps", "-aq", "--filter", f"label={label_filter}"])
        return [line.strip() for line in ids.splitlines() if line.strip()]

    @staticmethod
    def _label_value(label: str) -> str:
        """The VALUE half of a `key=value` label.

        Callers hold the full form (that is what `--filter label=` wants),
        while `docker inspect` reports bare values. Comparing the two directly
        never matches, which would silently defeat `reap_stale`'s own-run
        guard, so ownership checks go through here.
        """
        prefix = f"{LABEL_KEY}="
        return label[len(prefix):] if label.startswith(prefix) else label

    @staticmethod
    def _rm(container_ids: list[str]) -> None:
        for container_id in container_ids:
            subprocess.run(
                ["docker", "rm", "-f", container_id],
                capture_output=True, timeout=DOCKER_TIMEOUT, check=False,
            )

    @staticmethod
    def _owners(container_ids: list[str]) -> list[tuple[str, float, str]]:
        """`(id, age_seconds, label value)` for each id Docker still knows.

        `check=False` and the per-line skips are deliberate: a container can
        vanish between the `docker ps` that listed it and this call — a
        concurrent run reaping its own leftovers is exactly the situation this
        module now handles — and one unreadable entry must not abort the sweep.

        Anything unparseable is dropped, which means it is NOT reaped. That is
        the safe direction: a container whose age cannot be established might
        belong to a live session.
        """
        if not container_ids:
            return []
        proc = subprocess.run(
            ["docker", "inspect", "-f",
             '{{.Id}} {{.Created}} {{index .Config.Labels "' + LABEL_KEY + '"}}',
             *container_ids],
            capture_output=True, text=True, timeout=DOCKER_TIMEOUT, check=False,
        )
        now = datetime.now(timezone.utc)
        owners: list[tuple[str, float, str]] = []
        for line in proc.stdout.splitlines():
            parts = line.split(maxsplit=2)
            if len(parts) < 2:
                continue
            container_id, created = parts[0], parts[1]
            label = parts[2].strip() if len(parts) > 2 else ""
            # Docker stamps `Created` as RFC3339 with NANOSECOND precision,
            # which `datetime.fromisoformat` rejects. Second resolution is
            # ample against an hour-scale threshold, so parse the leading
            # `YYYY-MM-DDTHH:MM:SS` (always UTC) and discard the rest.
            try:
                created_at = datetime.strptime(
                    created[:19], "%Y-%m-%dT%H:%M:%S",
                ).replace(tzinfo=timezone.utc)
            except ValueError:
                continue
            owners.append((container_id, (now - created_at).total_seconds(), label))
        return owners

    @classmethod
    def reap_orphans(cls, label: str, *, min_age_secs: int = 0) -> None:
        """Remove containers carrying exactly `label`.

        atexit does not fire on SIGKILL or a hard crash, so a leaked container
        is possible; this clears it on the next run. Same exposure as the Rust
        plugin tests, whose testcontainers version cleans up via Drop.

        `docker ps -aq` matches running containers too, so this removes ANY
        container carrying `label` — including one that is still in active
        use. Callers MUST pass the label of a namespace they own, which is why
        `label` is required rather than defaulting to `cls.LABEL`: that
        constant is now only a prefix, so defaulting to it would silently match
        nothing. The default instance label is owned by construction (it
        carries RUN_ID, naming this process alone); cross-run cleanup belongs
        to `reap_stale`, which is age-gated for exactly this reason.

        `min_age_secs` spares containers younger than the given age. It
        defaults to 0 (remove every match) because an exactly-matching label is
        already proof of ownership.
        """
        ids = cls._ids_labelled(label)
        if min_age_secs <= 0:
            cls._rm(ids)
            return
        cls._rm([cid for cid, age, _ in cls._owners(ids) if age >= min_age_secs])

    @classmethod
    def stale_ids(
        cls,
        *,
        min_age_secs: int = REAP_MIN_AGE_SECS,
        own_label: str | None = None,
        run_id: str | None = RUN_ID,
    ) -> list[str]:
        """Ids `reap_stale` would remove, without removing them.

        Split out so the self-tests can assert the age decision — including at
        `min_age_secs=0` — without running a host-wide removal that would
        delete a live session's container, which is the very failure the age
        gate exists to prevent.

        Two exclusions, not one, because a label VALUE carries both a class
        prefix and a run identity. `own_label` spares that one exact value —
        what an explicit self-test label needs, since it carries no RUN_ID.
        `run_id` spares every value ending in `-{run_id}`, whatever its class
        prefix: one run can hold containers under more than one prefix
        (`usage-collector` and `usage-collector-ch`), and a second sidecar's
        `start()` sweep would otherwise treat the first one as foreign and
        reap it the moment age said so — a skewed daemon clock or a lowered
        `CF_GEARS_E2E_REAP_MIN_AGE_SECS` is enough, and under a pinned
        CF_GEARS_E2E_RUN_ID the two backend sessions are one run by
        definition. It defaults to this process's RUN_ID so no caller can
        forget it; pass `run_id=None` to sweep by age alone.
        """
        own = cls._label_value(own_label) if own_label is not None else None
        mine = f"-{run_id}" if run_id else None
        return [
            cid for cid, age, label in cls._owners(cls._ids_labelled(LABEL_KEY))
            if label != own
            and not (mine is not None and label.endswith(mine))
            and age >= min_age_secs
        ]

    @classmethod
    def reap_stale(
        cls,
        *,
        min_age_secs: int = REAP_MIN_AGE_SECS,
        own_label: str | None = None,
        run_id: str | None = RUN_ID,
    ) -> None:
        """Remove sidecar containers leaked by an earlier run of ANY session.

        Per-run label values stop sessions from reaping each other, but they
        also mean a crashed run's container no longer shares a label with
        anything that follows it, so nothing would ever clean it up. This sweep
        restores that: it matches the shared label KEY (every value) and then
        decides ownership by age.

        Age is a proxy, and it has to be: Docker cannot say whether the process
        that started a container is still alive, and a PID would be meaningless
        across hosts and containers. `min_age_secs` must therefore stay well
        above the longest plausible session — see REAP_MIN_AGE_SECS. A live
        concurrent run is younger than the threshold and survives; a container
        that has outlived any plausible session is leaked and goes.

        `own_label` and everything carrying `run_id` are skipped outright, so
        "never reap your own session" holds even if a skewed daemon clock made
        our own container look old — and holds for every backend the run
        started, not just the one calling this. See `stale_ids`.
        """
        cls._rm(cls.stale_ids(
            min_age_secs=min_age_secs, own_label=own_label, run_id=run_id,
        ))

    @classmethod
    def pull(cls) -> None:
        """Fetch IMAGE with a timeout sized for a cold cache.

        Every `docker run` of IMAGE MUST be preceded by this, because a `run`
        that pulls implicitly inherits its caller's much shorter timeout.
        `_run`'s DOCKER_TIMEOUT (120s) is fine for starting an already-local
        image but too tight if it also has to pull first, and a
        `subprocess.TimeoutExpired` during `docker run` escapes as a bare
        SubprocessError instead of this module's RuntimeError — before
        `atexit.register(self.stop)` has run, so the container (if the daemon
        managed to create it before the timeout) leaks until the next reap.
        Pulling first keeps `docker run` itself fast, and this is idempotent
        when the image is already present.
        """
        _run(["docker", "pull", cls.IMAGE], timeout=PULL_TIMEOUT)

    def start(self) -> None:
        require_docker()
        # Own namespace first (a re-run under a pinned CF_GEARS_E2E_RUN_ID, or a
        # test passing an explicit label, can have leftovers of its own), then
        # the age-gated sweep for containers leaked by other runs.
        self.reap_orphans(self.label)
        self.reap_stale(own_label=self.label)
        self.pull()

        self.container_id = _run([
            "docker", "run", "-d", "-P",
            "--label", self.label,
            *self._env_args(),
            self.IMAGE,
        ])
        atexit.register(self.stop)

        # Everything past `docker run` tears the container down on the way out.
        # `atexit` alone would leave it running for the rest of the session:
        # a raise here happens during session-fixture setup, so the
        # orchestrator's own `sc.stop()` — which sits after its `yield` — never
        # runs. BaseException, not Exception, so a Ctrl-C during the readiness
        # wait cleans up too.
        try:
            mapping = _run(["docker", "port", self.container_id, self.CONTAINER_PORT])
            # e.g. "0.0.0.0:32768" or two lines (IPv4 + IPv6). Take the first.
            self.port = int(mapping.splitlines()[0].rsplit(":", 1)[1])

            self._wait_ready()
        except BaseException:
            self.stop()
            raise

        print(f"[sidecar] {self.name} ready on 127.0.0.1:{self.port} "
              f"({self.container_id[:12]})")

    def _env_args(self) -> list[str]:
        """The `-e KEY=VALUE` pairs to pass to `docker run`."""
        raise NotImplementedError

    def _probe(self) -> tuple[bool, str]:
        """Run one readiness attempt: `(succeeded, error text when it did not)`.

        Called repeatedly by `_wait_ready`; must be cheap and must never raise.
        """
        raise NotImplementedError

    def _wait_ready(self) -> None:
        """Block until `_probe` succeeds `REQUIRED_CONSECUTIVE_OK` times running.

        A single success is not enough. A database container typically starts
        a temporary server to initialise itself, then shuts it down and starts
        the real one; a probe landing in that window succeeds against a server
        that is about to disappear, and the restart boundary can also accept a
        connection and immediately drop it. Requiring consecutive successes is
        what makes that boundary observable instead of a coin flip — hence the
        reset (not decrement) below: the boundary must be crossed cleanly, not
        merely survived on balance.
        """
        deadline = time.monotonic() + READY_TIMEOUT
        consecutive_ok = 0
        last_err = ""

        while time.monotonic() < deadline:
            ok, err = self._probe()
            if ok:
                consecutive_ok += 1
                if consecutive_ok >= REQUIRED_CONSECUTIVE_OK:
                    return
            else:
                consecutive_ok = 0
                last_err = err
            time.sleep(READY_POLL_INTERVAL)

        logs = subprocess.run(
            ["docker", "logs", "--tail", "80", self.container_id],
            capture_output=True, text=True, timeout=30, check=False,
        )
        raise RuntimeError(
            f"{self.name} not ready within {READY_TIMEOUT}s "
            f"(container {self.container_id}). Last probe error: {last_err}\n"
            f"--- docker logs ---\n{logs.stdout}\n{logs.stderr}"
        )

    def stop(self) -> None:
        if self.container_id is None:
            return
        subprocess.run(
            ["docker", "rm", "-f", self.container_id],
            capture_output=True, timeout=DOCKER_TIMEOUT, check=False,
        )
        self.container_id = None
        self.port = None


class TimescaleDbSidecar(_DockerSidecar):
    """A throwaway TimescaleDB container with a dynamically mapped host port.

    The image comes from the module-level pin (TIMESCALEDB_IMAGE) and
    `timescaledb_tag()`, so GEARS_TEST_TIMESCALEDB_TAG moves this lane and the
    Rust plugin tests together. Resolved once, at class-definition time: the
    variable is set before pytest starts, and keeping IMAGE a plain attribute
    spares every caller -- including test_sidecar_contract.py -- a method call.
    """

    IMAGE = f"{TIMESCALEDB_IMAGE}:{timescaledb_tag()}"
    LABEL = f"{LABEL_KEY}=usage-collector"
    CONTAINER_PORT = "5432/tcp"

    DB_USER = "uc"
    DB_PASSWORD = "uc"
    DB_NAME = "uc"

    name = "timescaledb"

    def _env_args(self) -> list[str]:
        return [
            "-e", f"POSTGRES_USER={self.DB_USER}",
            "-e", f"POSTGRES_PASSWORD={self.DB_PASSWORD}",
            "-e", f"POSTGRES_DB={self.DB_NAME}",
        ]

    def _probe(self) -> tuple[bool, str]:
        """Run a real query against the container's OWN TCP listener.

        `pg_isready` alone is NOT sufficient, and using it alone caused
        intermittent "connection reset by peer" / "expected to read 5 bytes,
        got 0 bytes at EOF" failures in the gear under test. Run over
        `docker exec`, it talks to the container's Unix socket, and the
        official Postgres entrypoint starts a *temporary* local-only server to
        run initdb and any init scripts before starting the real one — during
        that window `pg_isready` succeeds while nothing is listening on TCP
        yet.

        So readiness here means a real query, executed via `docker exec`
        against the container's own loopback (`-h 127.0.0.1 -p 5432`, i.e. the
        container's internal TCP listener, not the host-mapped port). The
        `-h 127.0.0.1` forces psql onto TCP rather than the Unix socket.
        Host-side reachability through the mapped port is a separate concern,
        covered by `test_sidecar_contract.py`'s plain `socket.create_connection`
        probe.
        """
        probe = subprocess.run(
            ["docker", "exec", "-e", f"PGPASSWORD={self.DB_PASSWORD}",
             self.container_id,
             "psql", "-h", "127.0.0.1", "-p", "5432",
             "-U", self.DB_USER, "-d", self.DB_NAME,
             "-tAc", "select 1"],
            capture_output=True, text=True, timeout=30, check=False,
        )
        if probe.returncode == 0 and probe.stdout.strip() == "1":
            return True, ""
        return False, (probe.stderr or probe.stdout).strip()


class ClickHouseSidecar(_DockerSidecar):
    """A throwaway ClickHouse container with a dynamically mapped host port.

    Keep IMAGE in sync with the Rust plugin integration tests:
    gears/system/usage-collector/plugins/clickhouse-usage-collector-plugin/tests/common/mod.rs
    A skew between the two means the plugin's schema DDL is validated against
    a different ClickHouse version than E2E runs.
    """

    IMAGE = "clickhouse/clickhouse-server:25.6"
    LABEL = f"{LABEL_KEY}=usage-collector-ch"
    # The HTTP interface: the `clickhouse` crate the plugin uses is
    # HTTP-based, so 8123 — not the native protocol's 9000 — is the port
    # `database_url` points at.
    CONTAINER_PORT = "8123/tcp"

    DB_USER = "default"
    # MUST be non-empty. The official image's entrypoint only provisions
    # `default` with `<networks><ip>::/0</ip></networks>` when CLICKHOUSE_USER
    # is non-default **or** CLICKHOUSE_PASSWORD is non-empty; otherwise it
    # writes a `users.d` override restricting `default` to 127.0.0.1/::1,
    # which rejects every connection arriving through the mapped host port.
    # Mirrors CH_TEST_PASSWORD in the Rust harness.
    DB_PASSWORD = "ch_test_pw"
    DB_NAME = "default"

    name = "clickhouse"

    def _env_args(self) -> list[str]:
        return [
            "-e", f"CLICKHOUSE_USER={self.DB_USER}",
            "-e", f"CLICKHOUSE_PASSWORD={self.DB_PASSWORD}",
            "-e", f"CLICKHOUSE_DB={self.DB_NAME}",
        ]

    def _probe(self) -> tuple[bool, str]:
        """Run `SELECT 1` over the mapped HTTP port, as the plugin itself will.

        Probing the host-mapped HTTP interface rather than exec'ing a client
        inside the container is deliberate: it is the exact transport, port
        and credentials the plugin's `build_client` uses, so a success here
        means the plugin can connect. That matters more than for TimescaleDB
        because the plugin's `apply_migrations` runs at gear `init` with no
        retry loop of its own (only the Rust *test* harness retries) — a
        false-ready therefore fails server startup outright rather than
        costing one reconnect.

        This image also logs to files under /var/log/clickhouse-server rather
        than stdout, so a log-based wait strategy cannot work at all; the Rust
        harness polls `SELECT 1` for the same reason.
        """
        import httpx

        try:
            resp = httpx.get(
                f"http://127.0.0.1:{self.port}/",
                params={"query": "SELECT 1"},
                auth=(self.DB_USER, self.DB_PASSWORD),
                timeout=5,
            )
        except httpx.HTTPError as exc:
            return False, f"{type(exc).__name__}: {exc}"
        if resp.status_code == 200 and resp.text.strip() == "1":
            return True, ""
        return False, f"HTTP {resp.status_code}: {resp.text.strip()[:200]}"
