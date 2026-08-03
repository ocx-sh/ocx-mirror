"""Shared fixtures and hooks for the mirror acceptance-test suite."""
from __future__ import annotations

import http.server
import json
import os
import re
import shutil
import sys
import threading
from pathlib import Path
from typing import NamedTuple
from uuid import uuid4

import pytest

from src.helpers import PROJECT_ROOT, start_registry
from src.mirror_runner import MirrorRunner
from src.runner import OcxRunner

SHFMT_FIXTURE_DIR = Path(__file__).resolve().parent / "fixtures" / "mirror-shfmt-minimal"

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
    registry = os.environ.get("REGISTRY", "localhost:5001")
    start_registry(registry)


# ---------------------------------------------------------------------------
# Session-scoped fixtures
# ---------------------------------------------------------------------------


@pytest.fixture(scope="session")
def registry() -> str:
    addr = os.environ.get("REGISTRY", "localhost:5001")
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
def mirror_binary() -> Path:
    """Path to the compiled ocx-mirror binary.

    Asserts rather than skips: a missing binary means the run built nothing,
    and a whole file skipping itself away is indistinguishable from a pass.
    """
    if env_path := os.environ.get("OCX_MIRROR_COMMAND"):
        p = Path(env_path)
    else:
        p = PROJECT_ROOT / "test" / "bin" / "ocx-mirror"
        if sys.platform == "win32" and not p.suffix:
            p = p.with_suffix(".exe")
    assert p.exists(), f"ocx-mirror binary not found at {p}"
    return p


# ---------------------------------------------------------------------------
# Function-scoped fixtures
# ---------------------------------------------------------------------------


@pytest.fixture()
def mirror(mirror_binary: Path, registry: str, tmp_path: Path) -> MirrorRunner:
    temp_dir = tmp_path / "mirror-work"
    temp_dir.mkdir()
    return MirrorRunner(mirror_binary, registry, temp_dir)


@pytest.fixture()
def unique_mirror_repo(request: pytest.FixtureRequest) -> str:
    """Generate a unique OCI repository name for mirror tests.

    The truncation is trailing-separator safe: an OCI repository name may not
    end in `_`, so a test whose name happens to have one at character 40 would
    otherwise publish into a repository every registry rejects with a 404 on
    the blob-upload endpoint — a failure that reads like a broken registry.
    """
    short_id = uuid4().hex[:8]
    name = re.sub(r"[^a-z0-9_]", "", request.node.name.lower())[:40].rstrip("_")
    return f"m_{short_id}_{name}"


@pytest.fixture()
def asset_server(tmp_path: Path):
    """Start a local HTTP server serving files from tmp_path/assets/.

    Every request is recorded on ``.requests`` as ``"<METHOD> <path>"``. That
    list is what makes "the patch fetched nothing upstream" an assertion rather
    than an inference: a run that downloaded is visible here, and the publish
    leg's own entries prove the recorder was live.
    """
    assets_dir = tmp_path / "assets"
    assets_dir.mkdir()
    requests: list[str] = []

    class Handler(http.server.SimpleHTTPRequestHandler):
        def send_head(self):
            # Both GET and HEAD route through here, so one override sees every
            # read of an upstream asset regardless of how it was requested.
            requests.append(f"{self.command} {self.path}")
            return super().send_head()

    httpd = http.server.HTTPServer(
        ("127.0.0.1", 0),
        lambda *args: Handler(*args, directory=str(assets_dir)),
    )
    port = httpd.server_address[1]
    thread = threading.Thread(target=httpd.serve_forever, daemon=True)
    thread.start()

    class Server:
        def __init__(self):
            self.dir = assets_dir
            self.port = port
            self.base_url = f"http://127.0.0.1:{port}"
            self.requests = requests

        def url(self, path: str) -> str:
            return f"{self.base_url}/{path}"

    yield Server()
    httpd.shutdown()


class WebhookCapture(NamedTuple):
    """Holds captured webhook POST requests."""

    url: str
    payloads: list[dict]


@pytest.fixture()
def webhook_server() -> WebhookCapture:
    """Local HTTP server standing in for the Discord webhook.

    Shared by the e2e (asserts the green embed is POSTed) and the pipeline
    suite (asserts an all-skipped run POSTs nothing) — silence is only a real
    assertion if something was listening.
    """
    captured: list[dict] = []

    class Handler(http.server.BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
            try:
                captured.append(json.loads(body))
            except json.JSONDecodeError:
                captured.append({"_raw": body.decode(errors="replace")})
            self.send_response(204)
            self.end_headers()

        def log_message(self, fmt: str, *args: object) -> None:  # noqa: ANN002
            pass  # suppress request logging in test output

    server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield WebhookCapture(url=f"http://127.0.0.1:{server.server_address[1]}/webhook", payloads=captured)
    server.shutdown()


@pytest.fixture()
def pipeline_spec(tmp_path: Path, registry: str, unique_mirror_repo: str, asset_server) -> Path:
    """Materialise the shfmt pipeline fixture with every placeholder resolved.

    The fixture ships three placeholders and all three must be substituted for
    the spec to describe a reachable world:

    - ``__ASSET_PORT__`` — the upstream asset URL. Left unsubstituted the
      source is unparseable and every command that touches it exits 69. That
      is what kept the e2e "skipped: plan unimplemented" for months.
    - ``localhost:5000`` — the target registry.
    - ``test-shfmt-pipeline`` — the target repository. Made unique per test so
      a second run cannot report ``skipped_existing`` off the first run's tags.

    The whole fixture directory is copied because ``metadata.default`` resolves
    relative to the spec's own directory.
    """
    spec_dir = tmp_path / "spec"
    shutil.copytree(SHFMT_FIXTURE_DIR, spec_dir)

    # The upstream asset the spec's url_index points at.
    (asset_server.dir / "shfmt_v3.7.0_linux_amd64").write_text("#!/bin/sh\necho v3.7.0\n")

    spec_path = spec_dir / "mirror.yml"
    spec_path.write_text(
        spec_path.read_text()
        .replace("__ASSET_PORT__", str(asset_server.port))
        .replace("localhost:5000", registry)
        .replace("test-shfmt-pipeline", unique_mirror_repo)
    )
    return spec_path


@pytest.fixture()
def ocx_home(tmp_path: Path) -> Path:
    home = tmp_path / "ocx-home"
    home.mkdir()
    return home


@pytest.fixture()
def ocx(ocx_binary: Path, ocx_home: Path, registry: str) -> OcxRunner:
    return OcxRunner(ocx_binary, ocx_home, registry)
