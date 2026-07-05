"""Shared fixtures and hooks for the mirror acceptance-test suite."""
from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

import pytest

from src.helpers import PROJECT_ROOT, start_registry
from src.runner import OcxRunner

# ---------------------------------------------------------------------------
# Session hooks
# ---------------------------------------------------------------------------


def pytest_sessionstart(session: pytest.Session) -> None:
    """Start the registry once before xdist workers spawn.

    Registry-independent opt-out (``OCX_TESTS_NO_REGISTRY=1``): selecting only
    tests that never touch a registry on a runner without Docker sets this
    flag so ``pytest_sessionstart`` does not hard-fail trying to
    ``docker compose up`` a registry no collected test needs.
    """
    if os.environ.get("PYTEST_XDIST_WORKER") is not None:
        return
    if os.environ.get("OCX_TESTS_NO_REGISTRY") == "1":
        return
    registry = os.environ.get("REGISTRY", "localhost:5000")
    start_registry(registry)


# ---------------------------------------------------------------------------
# Session-scoped fixtures
# ---------------------------------------------------------------------------


@pytest.fixture(scope="session")
def registry() -> str:
    addr = os.environ.get("REGISTRY", "localhost:5000")
    start_registry(addr)
    return addr


@pytest.fixture(scope="session")
def ocx_binary() -> Path:
    if env_path := os.environ.get("OCX_COMMAND"):
        p = Path(env_path)
    else:
        p = PROJECT_ROOT / "test" / "bin" / "ocx"
        if sys.platform == "win32" and not p.suffix:
            p = p.with_suffix(".exe")
    assert p.exists(), f"ocx binary not found at {p}"
    return p


@pytest.fixture(scope="session")
def real_ocx_binary() -> Path:
    """`ocx` built directly from the `external/ocx` submodule pin.

    Cross-repository blob-mount support (the `:from=` layer tail on
    `package push`, and the JSON `layers` push-report field it produces) is
    recent enough that whatever `ocx` resolves from `OCX_COMMAND`/`PATH` in
    this environment may predate it — build straight from the pinned
    submodule so mount-dependent tests exercise the real feature.
    """
    ocx_dir = PROJECT_ROOT / "external" / "ocx"
    binary = ocx_dir / "target" / "release" / "ocx"
    if not binary.exists():
        subprocess.run(["cargo", "build", "--release", "--bin", "ocx"], cwd=ocx_dir, check=True)
    assert binary.exists(), f"ocx binary not found at {binary} after build"
    return binary


# ---------------------------------------------------------------------------
# Function-scoped fixtures
# ---------------------------------------------------------------------------


@pytest.fixture()
def ocx_home(tmp_path: Path) -> Path:
    home = tmp_path / "ocx-home"
    home.mkdir()
    return home


@pytest.fixture()
def ocx(ocx_binary: Path, ocx_home: Path, registry: str) -> OcxRunner:
    return OcxRunner(ocx_binary, ocx_home, registry)
