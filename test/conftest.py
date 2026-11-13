"""Shared fixtures and hooks for the mirror acceptance-test suite."""
from __future__ import annotations

import http.server
import json
import os
import re
import shutil
import subprocess
import sys
import threading
import time
from pathlib import Path
from typing import NamedTuple
from uuid import uuid4

import pytest

from src.helpers import (
    PROJECT_ROOT,
    SIGSTORE_SERVICES,
    SigstoreStack,
    mint_identity_token,
    render_signing_fixture,
    sigstore_base_urls,
    sigstore_compose_argv,
    sigstore_compose_path,
    sigstore_skip_reason,
    sigstore_trusted_root,
    compose_up_service,
    docker_wanted,
    legacy_registry_address,
    registry_is_reachable,
    skip_without_docker,
    wait_for_sigstore,
)
from src import zot
from src.mirror_runner import MirrorRunner
from src.runner import OcxRunner

SHFMT_FIXTURE_DIR = Path(__file__).resolve().parent / "fixtures" / "mirror-shfmt-minimal"

# ---------------------------------------------------------------------------
# Session hooks
# ---------------------------------------------------------------------------


_ZOT = pytest.StashKey[zot.Stack]()


def _wants_zot(config: pytest.Config) -> bool:
    """Whether this process starts the session's zot registries.

    Not the xdist workers (they read the controller's ports from the
    environment), not a run that executes no test (`--collect-only`, `--help`,
    `--version`, `--fixtures`, `--markers`), and not a run that opted out.
    """
    if hasattr(config, "workerinput") or os.environ.get("OCX_TESTS_NO_REGISTRY") == "1":
        return False
    return not (
        config.getoption("collectonly")
        or config.getoption("help")
        or config.getoption("version")
        or config.getoption("showfixtures")
        or config.getoption("markers")
    )


def pytest_configure(config: pytest.Config) -> None:
    """Start the session's registries once, on the xdist controller, before workers spawn.

    The controller starts three zot instances (`src/zot.py`) and exports their
    `OCX_TEST_<NAME>_PORT`s; workers inherit the environment and never start
    one. The Sigstore stack comes up here too, and only when docker is wanted
    (`OCX_TEST_NO_DOCKER` unset): N workers racing one `docker compose up -d`
    is a container-creation race. Neither is stopped by the compose side; zot
    dies with the controller.
    """
    config.addinivalue_line(
        "markers",
        "requires_testing_build: asserts `__testing` provenance the release binary lacks; "
        "skipped when OCX_TEST_RELEASE_BINARY=1",
    )
    config.addinivalue_line(
        "markers",
        "requires_posix_shell: puts a `#!/bin/sh` stub on PATH for the mirror to spawn; "
        "skipped on Windows, where only .exe files spawn by name",
    )
    if not _wants_zot(config):
        return
    try:
        stack = zot.start()
    except zot.ZotError as error:
        raise pytest.UsageError(str(error)) from None
    config.stash[_ZOT] = stack
    os.environ.update(stack.env())
    # Workers inherit this; test_registry_port_belongs_to_the_session_zot reads it.
    os.environ["OCX_TEST_ZOT_PIDS"] = ",".join(map(str, stack.pids))
    if docker_wanted():
        _start_sigstore_stack()


def pytest_unconfigure(config: pytest.Config) -> None:
    if (stack := config.stash.get(_ZOT, None)) is not None:
        stack.stop()


def pytest_collection_modifyitems(items: list[pytest.Item]) -> None:
    """Skips `requires_testing_build` tests on release bytes and `requires_posix_shell` tests on Windows."""
    release = os.environ.get("OCX_TEST_RELEASE_BINARY") == "1"
    skip_release = pytest.mark.skip(
        reason="release bytes: the release binary lacks the `__testing` seams this test drives"
    )
    skip_windows = pytest.mark.skip(reason="windows: a `#!/bin/sh` stub on PATH cannot be spawned by name")
    for item in items:
        if release and item.get_closest_marker("requires_testing_build"):
            item.add_marker(skip_release)
        if sys.platform == "win32" and item.get_closest_marker("requires_posix_shell"):
            item.add_marker(skip_windows)


def _start_sigstore_stack() -> None:
    """Bring up the sibling checkout's `sigstore` profile, by explicit service name.

    Declines silently when there is nothing to bring up -- no sibling
    checkout, no generated trusted root: the `sigstore_stack` fixture is
    what turns that into a skip with a reason, per test that needs it.
    Naming the services keeps the sibling project's own registries, which
    collide with this harness's on 5001/5002, out of it.
    """
    if sigstore_skip_reason() is not None:
        return
    compose_file = sigstore_compose_path()
    brought_up = subprocess.run(
        [*sigstore_compose_argv(compose_file), "up", "-d", *SIGSTORE_SERVICES],
        capture_output=True,
        text=True,
    )
    if brought_up.returncode != 0:
        raise RuntimeError(
            f"docker compose up -d for the Sigstore profile in {compose_file} failed with "
            f"exit {brought_up.returncode}\nstdout: {brought_up.stdout.strip()}\n"
            f"stderr: {brought_up.stderr.strip()}"
        )


# ---------------------------------------------------------------------------
# Session-scoped fixtures
# ---------------------------------------------------------------------------


def _zot_address(instance: str) -> str:
    if os.environ.get("OCX_TESTS_NO_REGISTRY") == "1":
        pytest.skip(f"OCX_TESTS_NO_REGISTRY=1: {instance} registry not started")
    port = os.environ.get(f"OCX_TEST_{instance.upper()}_PORT")
    assert port, (
        f"no zot `{instance}` port in the environment: test/conftest.py::pytest_configure did not start one"
    )
    return f"localhost:{port}"


@pytest.fixture(scope="session")
def registry() -> str:
    return _zot_address("registry")


@pytest.fixture(scope="session")
def mirror_registry() -> str:
    """Destination registry for `registry sync` acceptance tests -- `registry` plays the upstream source."""
    return _zot_address("mirror")


@pytest.fixture(scope="session")
def zot_registry() -> str:
    """Native-Referrers-API registry (OCI Distribution 1.1), the session's third zot.

    `GET /v2/<name>/referrers/<digest>`, which `legacy_registry` (registry:2)
    does not implement (adr_mirror_signing.md D6, S-058).
    """
    return _zot_address("zot")


@pytest.fixture(scope="session")
def legacy_registry() -> str:
    """registry:2 through compose, the referrers-NEGATIVE and untagged-keeping leg.

    Skips only on a job that declares ``OCX_TEST_NO_DOCKER=1``. Without it a
    missing ``docker`` CLI, a dead daemon and a service that never answers all
    fail: a skip there would empty whole suites while the run still exits 0.
    """
    if os.environ.get("OCX_TESTS_NO_REGISTRY") == "1":
        pytest.skip("OCX_TESTS_NO_REGISTRY=1: legacy_registry not started")
    skip_without_docker("legacy_registry (the compose 'legacy_registry')")
    address = legacy_registry_address()
    if registry_is_reachable(address):
        return address
    if shutil.which("docker") is None:
        pytest.fail(
            f"legacy_registry: no registry:2 at {address} and no `docker` to start the compose "
            "service; a job without Docker declares OCX_TEST_NO_DOCKER=1",
            pytrace=False,
        )
    compose_up_service("legacy_registry")
    for _ in range(30):
        if registry_is_reachable(address):
            return address
        time.sleep(0.5)
    raise RuntimeError(f"legacy_registry at {address} did not become reachable")


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


@pytest.fixture(scope="session")
def real_ocx_binary() -> Path:
    """`ocx` built from the `external/ocx` submodule pin.

    Cross-repository blob-mount support (the `:from=` layer tail on
    `package push`, and the JSON `layers` push-report field it produces) is
    recent enough that whatever `ocx` resolves from `OCX_COMMAND`/`PATH` in
    this environment may predate it — mount-dependent tests need the pinned
    submodule's binary.

    CI provides it via ``OCX_TEST_BINARY`` (built and uploaded by the smoke
    job, whose checkout is ``submodules: recursive``; the acceptance job's is
    ``submodules: true`` — one level, enough for ocx's Sigstore compose file
    — so a cargo build here would see an empty ``external/ocx/external/*``
    and a dangling ``[patch.crates-io]``). Local dev falls back to building
    from the submodule directly.
    """
    if env_path := os.environ.get("OCX_TEST_BINARY"):
        p = Path(env_path)
        assert p.exists(), f"OCX_TEST_BINARY points at a missing file: {p}"
        return p
    ocx_dir = PROJECT_ROOT / "external" / "ocx"
    binary = ocx_dir / "target" / "release" / "ocx"
    if not binary.exists():
        subprocess.run(["cargo", "build", "--release", "--bin", "ocx"], cwd=ocx_dir, check=True)
    assert binary.exists(), f"ocx binary not found at {binary} after build"
    return binary


@pytest.fixture(scope="session")
def sigstore_stack(tmp_path_factory: pytest.TempPathFactory) -> SigstoreStack:
    """The running Sigstore stack, plus a spec and config rendered against it.

    Bring-up belongs to ``pytest_configure`` (controller-only, never torn
    down); this fixture only refuses to run when there is nothing to bring
    up, waits for readiness, mints the session's identity token and renders
    the fixture spec against both.

    The skip guard is real: a machine without a sibling `../ocx` checkout, or
    an `OCX_SIGSTORE_COMPOSE` pointed at a missing file, skips with a visible
    reason rather than failing or silently passing.
    """
    skip_without_docker("the Sigstore stack")
    reason = sigstore_skip_reason()
    if reason is not None:
        pytest.skip(reason)

    compose_file = sigstore_compose_path()
    wait_for_sigstore(compose_file)

    work = tmp_path_factory.mktemp("signing")
    token_path = mint_identity_token(work / "identity-token")
    spec_path = render_signing_fixture(
        work / "spec", sigstore_trusted_root(compose_file), token_path
    )
    base = sigstore_base_urls()
    return SigstoreStack(
        compose_file=compose_file,
        fulcio_url=base["fulcio"],
        rekor_url=base["rekor"],
        token_path=token_path,
        spec_path=spec_path,
        config_path=spec_path.parent / "config.toml",
    )


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


@pytest.fixture()
def published_index_server(tmp_path: Path):
    """Local HTTP server serving a hand-authored published-shape ocx-index tree.

    `registry sync`'s source read is a set of plain GETs against an `index:`
    base URL (`config.json`, `c/index.json`, `p/<ns>/<pkg>.json`) — this
    fixture is that base URL. Populate `.dir` with
    ``src.static_index.write_published_index_tree`` before pointing a spec's
    `index:` at ``.url()``; the handler reads from disk per request, so
    writing the tree before or after the server starts is equally fine.

    Every request is recorded on ``.requests`` as ``"<METHOD> <path>"``, the
    same shape ``asset_server`` uses. Two scenarios need it: the no-op re-run
    (S-002) counts catalog fetches, and the SSRF refusal (S-007) proves the
    run reached the root document before refusing — "zero registry requests"
    otherwise only proves nothing ran at all.
    """
    index_dir = tmp_path / "index"
    index_dir.mkdir()
    requests: list[str] = []

    class Handler(http.server.SimpleHTTPRequestHandler):
        def send_head(self):
            # Both GET and HEAD route through here, so one override sees every
            # read of the served tree regardless of how it was requested.
            requests.append(f"{self.command} {self.path}")
            return super().send_head()

    httpd = http.server.HTTPServer(
        ("127.0.0.1", 0),
        lambda *args: Handler(*args, directory=str(index_dir)),
    )
    thread = threading.Thread(target=httpd.serve_forever, daemon=True)
    thread.start()

    class Server:
        def __init__(self):
            self.dir = index_dir
            self.base_url = f"http://127.0.0.1:{httpd.server_address[1]}"
            self.requests = requests

        def url(self, path: str = "") -> str:
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
