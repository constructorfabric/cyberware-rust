"""Self-managed lifecycle for the out-of-process (loopback) E2E suite.

Boots the platform-host (edge + DirectoryService) and three OoP gear processes
on loopback, waits for the edge to sync their routes from the DirectoryService,
and yields the edge base URL. Everything is torn down on session teardown.

No Kubernetes: this is the same software path as the cluster demo (OoP
bootstrap, directory-resolved REST clients, edge reverse-proxy) with processes
on 127.0.0.1 instead of pods. Heavy work (cargo build, process boot, route
sync) happens in the session fixture; run pytest with `-o timeout_func_only=true`
so the per-test timeout does not count fixture setup (the `make e2e-oop` target
does this).
"""
from __future__ import annotations

import os
import socket
import subprocess
import time
from dataclasses import dataclass
from pathlib import Path

import httpx
import pytest

# testing/e2e/suites/oop/conftest.py -> repo root
ROOT = Path(__file__).resolve().parents[4]
TARGET_DIR = ROOT / "target" / "debug"
# Reuse the git-ignored shared E2E logs dir; prefix files so they are distinct.
LOG_DIR = ROOT / "testing" / "e2e" / "logs"
LOG_PREFIX = "oop-"

EDGE_PORT = 8087
DIRECTORY_PORT = 50051
DIRECTORY_ENDPOINT = f"http://127.0.0.1:{DIRECTORY_PORT}"
BASE_URL = f"http://127.0.0.1:{EDGE_PORT}"
# static-authn accept_all maps any non-empty bearer to the platform-root tenant.
TOKEN = os.environ.get("OOP_E2E_TOKEN", "oop-e2e-token")

BUILD_TIMEOUT = int(os.environ.get("OOP_E2E_BUILD_TIMEOUT", "1800"))
HOST_HEALTH_TIMEOUT = int(os.environ.get("OOP_E2E_HOST_TIMEOUT", "120"))
ROUTE_SYNC_TIMEOUT = int(os.environ.get("OOP_E2E_ROUTE_TIMEOUT", "60"))


@dataclass
class Binary:
    """A binary to build and (optionally) launch."""
    name: str            # cargo bin name == target/debug/<name>
    package: str         # cargo package (-p)
    features: str        # comma-separated cargo features ("" = none)


@dataclass
class Proc:
    """A launched process + its log file handle."""
    name: str
    popen: subprocess.Popen
    log_fh: object


# platform-host binary + the three OoP gear binaries.
HOST = Binary(name="platform-host", package="cf-gears-platform-host", features="")
GEARS = [
    Binary(name="hello-oop", package="hello", features="oop_module"),
    Binary(name="api-contracts-oop", package="cf-api-contracts", features="oop_module"),
    Binary(
        name="api-contracts-consumer-oop",
        package="cf-api-contracts-consumer",
        features="oop_module",
    ),
]

# Launch specs: (binary name, config path, is_host).
HOST_CONFIG = "config/oop-host.yaml"
GEAR_LAUNCH = [
    ("hello-oop", "config/oop-hello.yaml"),
    ("api-contracts-oop", "config/oop-api-contracts.yaml"),
    ("api-contracts-consumer-oop", "config/oop-api-contracts-consumer.yaml"),
]


# ── helpers ────────────────────────────────────────────────────────────────

def _port_in_use(port: int) -> bool:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.settimeout(0.5)
        return s.connect_ex(("127.0.0.1", port)) == 0


def _cargo_build(bin_: Binary) -> None:
    cmd = ["cargo", "build", "-p", bin_.package, "--bin", bin_.name]
    if bin_.features:
        cmd += ["--features", bin_.features]
    print(f"[oop-e2e] building {bin_.name} ({' '.join(cmd)})")
    subprocess.run(cmd, cwd=str(ROOT), check=True, timeout=BUILD_TIMEOUT)


def _ensure_built() -> None:
    force = os.environ.get("OOP_E2E_FORCE_BUILD") == "1"
    for b in [HOST, *GEARS]:
        path = TARGET_DIR / b.name
        if force or not path.exists():
            _cargo_build(b)
        if not path.exists():
            pytest.fail(f"binary not produced: {path}")


def _launch(bin_name: str, config: str, *, is_host: bool, child_home: str) -> Proc:
    LOG_DIR.mkdir(parents=True, exist_ok=True)
    log_path = LOG_DIR / f"{LOG_PREFIX}{bin_name}.log"
    log_fh = open(log_path, "w")
    # Isolate on-disk state (SQLite DBs live under `~/.cf-gears-*`) into a
    # throwaway HOME so runs are repeatable and the real home stays clean.
    env = {**os.environ, "HOME": child_home, "RUST_LOG": os.environ.get("RUST_LOG", "info")}
    env["TOOLKIT_DIRECTORY_ENDPOINT"] = DIRECTORY_ENDPOINT
    cmd = [str(TARGET_DIR / bin_name), "--config", config]
    if is_host:
        cmd.append("run")  # the host binary uses a `run` subcommand
    print(f"[oop-e2e] launching {bin_name} -> {log_path}")
    try:
        popen = subprocess.Popen(
            cmd, cwd=str(ROOT), stdout=log_fh, stderr=subprocess.STDOUT, env=env
        )
    except Exception:
        log_fh.close()
        raise
    return Proc(name=bin_name, popen=popen, log_fh=log_fh)


def _tail(bin_name: str, n: int = 60) -> str:
    p = LOG_DIR / f"{LOG_PREFIX}{bin_name}.log"
    if not p.exists():
        return "(no log)"
    return "".join(p.read_text(errors="replace").splitlines(keepends=True)[-n:])


def _poll(desc: str, fn, *, timeout: int, procs: list[Proc]) -> None:
    """Poll fn() until it returns True; fail with log tails on timeout/crash."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        for pr in procs:
            if pr.popen.poll() is not None:
                pytest.fail(
                    f"process {pr.name} exited (code {pr.popen.returncode}) "
                    f"while waiting for {desc}.\n--- {pr.name} log ---\n{_tail(pr.name)}"
                )
        try:
            if fn():
                return
        except httpx.HTTPError:
            pass
        time.sleep(1)
    tails = "\n".join(f"--- {pr.name} log ---\n{_tail(pr.name)}" for pr in procs)
    pytest.fail(f"timed out after {timeout}s waiting for {desc}.\n{tails}")


def _status(method: str, path: str, **kw) -> int:
    try:
        r = httpx.request(method, f"{BASE_URL}{path}", timeout=3, **kw)
        return r.status_code
    except httpx.HTTPError:
        return 0


# ── session fixture ─────────────────────────────────────────────────────────

@pytest.fixture(scope="session")
def oop_cluster(tmp_path_factory):
    """Build, boot, and tear down the loopback OoP cluster; yield the base URL."""
    if os.environ.get("OOP_E2E_SKIP") == "1":
        pytest.skip("OOP_E2E_SKIP=1")
    for port in (EDGE_PORT, DIRECTORY_PORT):
        if _port_in_use(port):
            pytest.skip(
                f"port {port} already in use — another server is running; "
                f"stop it or set OOP_E2E_SKIP=1"
            )

    _ensure_built()
    child_home = str(tmp_path_factory.mktemp("oop-home"))

    procs: list[Proc] = []
    try:
        # 1) platform-host (edge + DirectoryService)
        host = _launch("platform-host", HOST_CONFIG, is_host=True, child_home=child_home)
        procs.append(host)
        _poll(
            "edge /healthz",
            lambda: _status("GET", "/healthz") == 200,
            timeout=HOST_HEALTH_TIMEOUT,
            procs=procs,
        )

        # 2) OoP gears (register with the DirectoryService)
        for bin_name, cfg in GEAR_LAUNCH:
            procs.append(_launch(bin_name, cfg, is_host=False, child_home=child_home))

        # 3) wait for the edge to sync each gear's routes from the directory
        _poll(
            "hello route synced at edge",
            lambda: _status("GET", "/hello/v1/ping") == 200,
            timeout=ROUTE_SYNC_TIMEOUT,
            procs=procs,
        )
        _poll(
            "consumer route synced at edge",
            lambda: _status(
                "POST",
                "/api-contracts-consumer/v1/charge",
                headers={"Authorization": f"Bearer {TOKEN}"},
                json={"amount_cents": 1, "currency": "USD", "description": "warmup"},
            ) not in (0, 404),
            timeout=ROUTE_SYNC_TIMEOUT,
            procs=procs,
        )

        yield BASE_URL
    finally:
        for pr in reversed(procs):
            pr.popen.terminate()
        for pr in reversed(procs):
            try:
                pr.popen.wait(timeout=5)
            except subprocess.TimeoutExpired:
                pr.popen.kill()
                try:
                    pr.popen.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    pass
            try:
                pr.log_fh.close()
            except Exception:
                pass


@pytest.fixture(scope="session")
def auth():
    """Bearer header accepted by static-authn accept_all."""
    return {"Authorization": f"Bearer {TOKEN}"}
