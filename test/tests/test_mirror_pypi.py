"""W3: acceptance tests for the `source.type: pypi` pipeline.

`source.type: pypi` discovers upstream versions from a PyPI-compatible JSON
index and derives a PEP 751 lock in-pipeline per candidate version (`uv pip
compile` against a materialized interpreter — see `pipeline/lock_derive.rs`
and `command/package/pipeline/plan.rs::build_pypi_plan_entries`). This suite
follows the same hermetic-stubbing conventions as `test_mirror_pylock.py`:

- The target registry is a tiny local HTTP stub that 404s every request as
  `NAME_UNKNOWN`, so `pipeline plan`'s fail-safe "first publish" branch is
  taken without a real OCI registry (negative cases only — registry-free).
- `uv` is stubbed via `OCX_MIRROR_UV` pointing at a shell script that emits a
  canned `pylock.toml` (mirrors `lock_derive.rs`'s own `write_uv_stub` test
  helper).
- The pinned interpreter's materialization (`ocx package pull`, shelled by
  `materialize_interpreter`) is stubbed via `OCX_BINARY_PIN` pointing at a
  script that echoes a canned JSON mapping straight to a local directory
  already containing `content/bin/python3` — no registry interaction.

The positive case additionally needs `pipeline prepare` to resolve the pinned
interpreter's manifest *digest*, which is an in-process registry call
(`ocx_lib`'s OCI client, not a subprocess) and therefore cannot be stubbed via
`OCX_BINARY_PIN`. It runs against the real `registry` fixture, with a
throwaway interpreter package pushed there via the real `ocx` binary
(`real_ocx_binary`/`push_stub_ocx_package`, conftest.py) — the content is a
one-byte marker; nothing downstream ever executes it.
"""
from __future__ import annotations

import base64
import hashlib
import http.server
import json
import os
import re
import socket
import stat
import subprocess
import threading
import uuid
from pathlib import Path

import pytest

from src.helpers import PROJECT_ROOT, push_stub_ocx_package

FIXTURE_WHEEL = PROJECT_ROOT / "crates" / "ocx_python" / "tests" / "fixtures" / "wheels" / "console_pkg-1.0.0-py3-none-any.whl"

# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


class _RepositoryNotFoundHandler(http.server.BaseHTTPRequestHandler):
    """Minimal OCI-registry stub: every request 404s as NAME_UNKNOWN.

    Same rationale as `test_mirror_pylock.py`'s stub — `pipeline plan` reads
    the target registry before the source, and the fail-safe path treats an
    authoritative 404 as "nothing published yet" rather than aborting.
    """

    def do_GET(self) -> None:  # noqa: N802 (BaseHTTPRequestHandler API)
        body = json.dumps(
            {"errors": [{"code": "NAME_UNKNOWN", "message": "repository name not known to registry"}]}
        ).encode()
        self.send_response(404)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, fmt: str, *args: object) -> None:  # noqa: ANN002
        pass


@pytest.fixture()
def stub_registry() -> str:
    """Starts the local 404-everything OCI stub. Yields its ``host:port``."""
    server = http.server.HTTPServer(("127.0.0.1", 0), _RepositoryNotFoundHandler)
    port = server.server_address[1]
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    yield f"127.0.0.1:{port}"
    server.shutdown()


def _make_pypi_handler(
    projects: dict[str, bytes],
    wheels: dict[str, bytes],
    credentials: tuple[str, str] | None = None,
) -> type:
    """Simple Repository API stand-in (PEP 503 project URL, PEP 691 JSON body).

    ``credentials`` makes it an authenticated index: every request without the
    matching HTTP Basic header answers 401, which is what a corporate Nexus or
    Artifactory does.
    """
    expected_auth = None
    if credentials is not None:
        user, secret = credentials
        expected_auth = "Basic " + base64.b64encode(f"{user}:{secret}".encode()).decode()

    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self) -> None:  # noqa: N802
            if expected_auth is not None and self.headers.get("Authorization") != expected_auth:
                self.send_response(401)
                self.send_header("WWW-Authenticate", 'Basic realm="pypi"')
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            parts = self.path.strip("/").split("/")
            if len(parts) == 1 and parts[0] in projects:
                self._send(200, projects[parts[0]], "application/vnd.pypi.simple.v1+json")
                return
            if len(parts) == 2 and parts[0] == "wheels" and parts[1] in wheels:
                self._send(200, wheels[parts[1]], "application/octet-stream")
                return
            self._send(404, b"not found", "text/plain")

        def _send(self, code: int, body: bytes, content_type: str) -> None:
            self.send_response(code)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, fmt: str, *args: object) -> None:  # noqa: ANN002
            pass

    return Handler


def _start_fake_pypi(
    projects: dict[str, bytes],
    wheels: dict[str, bytes],
    credentials: tuple[str, str] | None = None,
) -> tuple[str, http.server.HTTPServer]:
    """Starts a local stand-in for a Simple API index. Returns its base URL + server."""
    server = http.server.HTTPServer(("127.0.0.1", 0), _make_pypi_handler(projects, wheels, credentials))
    port = server.server_address[1]
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return f"http://127.0.0.1:{port}", server


def _unreachable_index() -> str:
    """A `host:port` guaranteed to refuse connections: bind, grab the port, close.

    Same technique as `source::pypi`'s own
    `classify_error_maps_connection_refused_to_source_error` Rust unit test.
    """
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.bind(("127.0.0.1", 0))
    port = sock.getsockname()[1]
    sock.close()
    return f"http://127.0.0.1:{port}"


def _project_json(releases: dict[str, list[dict[str, object]]]) -> bytes:
    """Renders a PEP 691 project page from ``{version: [file, ...]}``.

    The input shape is kept version-keyed because that is how the tests read;
    the Simple API itself lists only files, and versions are derived from their
    filenames (`source::pypi`).
    """
    files = [
        {"filename": entry["filename"], "url": f"../wheels/{entry['filename']}", "hashes": {}, "yanked": entry.get("yanked", False)}
        for entries in releases.values()
        for entry in entries
    ]
    return json.dumps({"meta": {"api-version": "1.0"}, "files": files}).encode()


def _write_executable(path: Path, body: str) -> None:
    path.write_text(body)
    path.chmod(path.stat().st_mode | stat.S_IEXEC | stat.S_IXGRP | stat.S_IXOTH)


def _write_ocx_pull_stub(tmp_path: Path, interpreter_package: str) -> Path:
    """Stub for `materialize_interpreter`'s `ocx package pull` subprocess call.

    Echoes a canned JSON mapping straight to a local directory that already
    contains `content/bin/python3` — no registry interaction, mirrors
    `lock_derive.rs`'s own unit-test stub.
    """
    interpreter_root = tmp_path / "interpreter-root"
    bin_dir = interpreter_root / "content" / "bin"
    bin_dir.mkdir(parents=True)
    (bin_dir / "python3").write_text("")

    stub = tmp_path / "ocx-pull-stub.sh"
    payload = json.dumps({interpreter_package: str(interpreter_root)})
    _write_executable(stub, f"#!/bin/sh\necho '{payload}'\n")
    return stub


def _write_uv_stub(tmp_path: Path, name: str, body: str, exit_code: int) -> Path:
    """Stub `uv`: consumes stdin, finds the `-o <path>` arg, writes `body`
    there, then exits with `exit_code`. Mirrors `lock_derive.rs`'s own
    `write_uv_stub` test helper exactly."""
    script = (
        "#!/bin/sh\n"
        "cat > /dev/null\n"
        'prev=""\n'
        'outfile=""\n'
        'for arg in "$@"; do\n'
        '  if [ "$prev" = "-o" ]; then outfile="$arg"; fi\n'
        '  prev="$arg"\n'
        "done\n"
        'if [ -n "$outfile" ]; then cat > "$outfile" <<LOCKEOF\n'
        f"{body}"
        "LOCKEOF\n"
        "fi\n"
        f"exit {exit_code}\n"
    )
    stub = tmp_path / name
    _write_executable(stub, script)
    return stub


def _canned_pylock(package: str, version: str, wheel_filename: str, wheel_url: str, sha256: str) -> str:
    return (
        'lock-version = "1.0"\n'
        'requires-python = ">=3.9"\n'
        "\n"
        "[[packages]]\n"
        f'name = "{package}"\n'
        f'version = "{version}"\n'
        "\n"
        "[[packages.wheels]]\n"
        f'name = "{wheel_filename}"\n'
        f'url = "{wheel_url}"\n'
        f'hashes = {{ sha256 = "{sha256}" }}\n'
    )


def _write_spec(
    tmp_path: Path,
    *,
    registry: str,
    repository: str,
    package: str,
    index: str,
    interpreter_package: str,
    build_timestamp: str | None = None,
) -> Path:
    stamp = f"\nbuild_timestamp: {build_timestamp}\n" if build_timestamp else ""
    spec = f"""name: {package}
target:
  registry: {registry}
  repository: {repository}

source:
  type: pypi
  package: {package}
  indexes:
    - url: {index}

python:
  version: "3.13.1"
  abi: cp313
  interpreter_package: "{interpreter_package}"
{stamp}
wheels:
  linux/amd64: ~

platforms:
  linux/amd64:
    runner: ubuntu-latest
"""
    spec_path = tmp_path / "mirror.yml"
    spec_path.write_text(spec)
    return spec_path


def _run_mirror(mirror_binary: Path, args: list[str], env: dict[str, str]) -> subprocess.CompletedProcess[str]:
    full_env = {**os.environ, **env}
    return subprocess.run([str(mirror_binary), *args], capture_output=True, text=True, env=full_env)


# ---------------------------------------------------------------------------
# Negative cases — exact exit codes are the contract
# ---------------------------------------------------------------------------


def test_plan_rejects_unknown_package_with_404(mirror_binary: Path, stub_registry: str, tmp_path: Path) -> None:
    """A PyPI 404 (unknown package name) is malformed input: PypiError, exit 65."""
    index, server = _start_fake_pypi(projects={}, wheels={})
    try:
        spec_path = _write_spec(
            tmp_path,
            registry=stub_registry,
            repository="pypi-404",
            package="missing-pkg",
            index=index,
            interpreter_package="ocx.sh/python/cpython:3.13.1",
        )
        result = _run_mirror(
            mirror_binary,
            ["package", "pipeline", "plan", "--spec", str(spec_path)],
            {"OCX_INSECURE_REGISTRIES": stub_registry},
        )
    finally:
        server.shutdown()

    assert result.returncode == 65, f"expected exit 65 (DataError) for unknown package, got {result.returncode}\nstderr: {result.stderr}"
    assert "missing-pkg" in result.stderr, f"error must name the offending package: {result.stderr}"


def test_plan_maps_unreachable_index_to_unavailable(mirror_binary: Path, stub_registry: str, tmp_path: Path) -> None:
    """A connection-refused index is a transient resource failure: SourceError, exit 69."""
    spec_path = _write_spec(
        tmp_path,
        registry=stub_registry,
        repository="pypi-unreachable",
        package="whatever",
        index=_unreachable_index(),
        interpreter_package="ocx.sh/python/cpython:3.13.1",
    )
    result = _run_mirror(
        mirror_binary,
        ["package", "pipeline", "plan", "--spec", str(spec_path)],
        {"OCX_INSECURE_REGISTRIES": stub_registry},
    )

    assert result.returncode == 69, f"expected exit 69 (Unavailable) for unreachable index, got {result.returncode}\nstderr: {result.stderr}"


def test_plan_authenticates_to_a_private_index(mirror_binary: Path, stub_registry: str, tmp_path: Path) -> None:
    """A credential-gated index is reachable through either auth rung, and unreachable without one.

    Both rungs are host-keyed — `OCX_AUTH_<slug>_*` (the slug is the host with
    every non-alphanumeric byte replaced, so `127.0.0.1` reads
    `OCX_AUTH_127_0_0_1_*`) and netrc's `machine` entry. Nothing about the
    credential appears in `mirror.yml`.
    """
    releases = {"1.0.0": [{"filename": "acme_app-1.0.0-py3-none-any.whl", "yanked": False}]}
    index, server = _start_fake_pypi(
        projects={"acme-app": _project_json(releases)},
        wheels={},
        credentials=("ci-mirror", "s3cr3t"),
    )
    try:
        interpreter_package = "ocx.sh/python/cpython:3.13.1"
        ocx_stub = _write_ocx_pull_stub(tmp_path, interpreter_package)
        uv_stub = tmp_path / "uv-fail.sh"
        _write_executable(uv_stub, "#!/bin/sh\ncat > /dev/null\necho 'resolution failed' >&2\nexit 1\n")

        spec_path = _write_spec(
            tmp_path,
            registry=stub_registry,
            repository="pypi-private-index",
            package="acme-app",
            index=index,
            interpreter_package=interpreter_package,
        )
        plan = ["package", "pipeline", "plan", "--spec", str(spec_path), "--locks-dir", str(tmp_path / "locks")]
        # A netrc the developer's own ~/.netrc cannot stand in for.
        absent_netrc = str(tmp_path / "no-such-netrc")

        anonymous = _run_mirror(
            mirror_binary,
            plan,
            {"OCX_INSECURE_REGISTRIES": stub_registry, "NETRC": absent_netrc},
        )

        env_auth = _run_mirror(
            mirror_binary,
            plan,
            {
                "OCX_INSECURE_REGISTRIES": stub_registry,
                "NETRC": absent_netrc,
                "OCX_AUTH_127_0_0_1_USER": "ci-mirror",
                "OCX_AUTH_127_0_0_1_TOKEN": "s3cr3t",
                "OCX_BINARY_PIN": str(ocx_stub),
                "OCX_MIRROR_UV": str(uv_stub),
            },
        )

        netrc_path = tmp_path / "netrc"
        netrc_path.write_text("machine 127.0.0.1 login ci-mirror password s3cr3t\n")
        netrc_path.chmod(0o600)
        netrc_auth = _run_mirror(
            mirror_binary,
            plan,
            {
                "OCX_INSECURE_REGISTRIES": stub_registry,
                "NETRC": str(netrc_path),
                "OCX_BINARY_PIN": str(ocx_stub),
                "OCX_MIRROR_UV": str(uv_stub),
            },
        )
    finally:
        server.shutdown()

    # 401 is "unknown", not "absent": SourceError (69), never the 65 an unknown
    # package name gets — a wrong credential must not read as a typo'd package.
    assert anonymous.returncode == 69, (
        f"expected exit 69 (Unavailable) for an unauthenticated index, got {anonymous.returncode}"
        f"\nstderr: {anonymous.stderr}"
    )
    for label, result in (("OCX_AUTH", env_auth), ("netrc", netrc_auth)):
        assert result.returncode == 65, (
            f"{label}: discovery must get past the index and reach lock derivation, "
            f"got {result.returncode}\nstderr: {result.stderr}"
        )
        assert "resolution failed" in result.stderr, f"{label}: expected the uv stub's failure, got: {result.stderr}"
    assert "s3cr3t" not in env_auth.stderr and "s3cr3t" not in netrc_auth.stderr, "no credential may reach stderr"


def test_plan_maps_uv_nonzero_exit_to_data_error(mirror_binary: Path, stub_registry: str, tmp_path: Path) -> None:
    """A `uv pip compile` resolution failure is malformed lock content: PylockError, exit 65."""
    releases = {"1.0.0": [{"filename": "acme_app-1.0.0-py3-none-any.whl", "yanked": False}]}
    index, server = _start_fake_pypi(projects={"acme-app": _project_json(releases)}, wheels={})
    try:
        interpreter_package = "ocx.sh/python/cpython:3.13.1"
        ocx_stub = _write_ocx_pull_stub(tmp_path, interpreter_package)
        uv_stub = tmp_path / "uv-fail.sh"
        _write_executable(
            uv_stub, "#!/bin/sh\ncat > /dev/null\necho 'resolution failed' >&2\nexit 1\n"
        )

        spec_path = _write_spec(
            tmp_path,
            registry=stub_registry,
            repository="pypi-uv-fail",
            package="acme-app",
            index=index,
            interpreter_package=interpreter_package,
        )
        result = _run_mirror(
            mirror_binary,
            ["package", "pipeline", "plan", "--spec", str(spec_path), "--locks-dir", str(tmp_path / "locks")],
            {
                "OCX_INSECURE_REGISTRIES": stub_registry,
                "OCX_BINARY_PIN": str(ocx_stub),
                "OCX_MIRROR_UV": str(uv_stub),
            },
        )
    finally:
        server.shutdown()

    assert result.returncode == 65, f"expected exit 65 (DataError) for uv resolution failure, got {result.returncode}\nstderr: {result.stderr}"
    assert "resolution failed" in result.stderr, f"error must surface uv's stderr: {result.stderr}"


def test_plan_maps_missing_uv_binary_to_execution_failed(mirror_binary: Path, stub_registry: str, tmp_path: Path) -> None:
    """A missing `uv` binary is a subprocess-execution failure: ExecutionFailed, exit 1."""
    releases = {"1.0.0": [{"filename": "acme_app-1.0.0-py3-none-any.whl", "yanked": False}]}
    index, server = _start_fake_pypi(projects={"acme-app": _project_json(releases)}, wheels={})
    try:
        interpreter_package = "ocx.sh/python/cpython:3.13.1"
        ocx_stub = _write_ocx_pull_stub(tmp_path, interpreter_package)
        missing_uv = tmp_path / "no-such-uv"

        spec_path = _write_spec(
            tmp_path,
            registry=stub_registry,
            repository="pypi-uv-missing",
            package="acme-app",
            index=index,
            interpreter_package=interpreter_package,
        )
        result = _run_mirror(
            mirror_binary,
            ["package", "pipeline", "plan", "--spec", str(spec_path), "--locks-dir", str(tmp_path / "locks")],
            {
                "OCX_INSECURE_REGISTRIES": stub_registry,
                "OCX_BINARY_PIN": str(ocx_stub),
                "OCX_MIRROR_UV": str(missing_uv),
            },
        )
    finally:
        server.shutdown()

    assert result.returncode == 1, f"expected exit 1 (Failure) for a missing uv binary, got {result.returncode}\nstderr: {result.stderr}"
    assert "failed to spawn uv" in result.stderr, f"error must name the spawn failure: {result.stderr}"


# ---------------------------------------------------------------------------
# Positive: full plan -> prepare against a pypi fixture
# ---------------------------------------------------------------------------


def test_plan_then_prepare_produces_env_bundle(
    mirror_binary: Path,
    real_ocx_binary: Path,
    registry: str,
    tmp_path: Path,
) -> None:
    """Full `plan` -> `prepare` against a pypi fixture: a derived lock lands in
    `locks/`, `plan.json` references it, and `prepare --plan` produces an env
    bundle (metadata.json + wheel layer + env-manifest.json)."""
    unique = uuid.uuid4().hex[:8]
    package = "console-pkg"

    # `resolve_interpreter_dependencies` (prepare) fetches the interpreter's
    # manifest digest directly against the registry (in-process OCI client
    # call, not a subprocess) — push a throwaway stand-in for real.
    interpreter_ref = f"python/cpython-pypi-{unique}:3.13.1"
    push_stub_ocx_package(real_ocx_binary, registry, interpreter_ref, tmp_path / "push-setup")
    interpreter_package = f"{registry}/{interpreter_ref}"

    wheel_filename = "console_pkg-1.0.0-py3-none-any.whl"
    wheel_bytes = FIXTURE_WHEEL.read_bytes()
    sha256 = hashlib.sha256(wheel_bytes).hexdigest()

    releases = {"1.0.0": [{"filename": wheel_filename, "yanked": False}]}
    index, server = _start_fake_pypi(
        projects={package: _project_json(releases)},
        wheels={wheel_filename: wheel_bytes},
    )
    try:
        ocx_stub = _write_ocx_pull_stub(tmp_path, interpreter_package)
        wheel_url = f"{index}/wheels/{wheel_filename}"
        uv_stub = _write_uv_stub(
            tmp_path,
            "uv-ok.sh",
            _canned_pylock(package, "1.0.0", wheel_filename, wheel_url, sha256),
            0,
        )

        spec_path = _write_spec(
            tmp_path,
            registry=registry,
            repository=f"pypi-e2e-{unique}",
            package=package,
            index=index,
            interpreter_package=interpreter_package,
            # `build_timestamp` was silently ignored on the env path: the plan
            # stamped nothing, so every re-publish re-pointed the bare `X.Y.Z`
            # tag and its whole cascade at a fresh digest.
            build_timestamp="date",
        )
        env = {
            "OCX_INSECURE_REGISTRIES": registry,
            "OCX_BINARY_PIN": str(ocx_stub),
            "OCX_MIRROR_UV": str(uv_stub),
        }
        locks_dir = tmp_path / "locks"

        plan_result = _run_mirror(
            mirror_binary,
            [
                "package",
                "pipeline",
                "plan",
                "--spec",
                str(spec_path),
                "--locks-dir",
                str(locks_dir),
                "--format",
                "json",
            ],
            env,
        )
        assert plan_result.returncode == 0, f"plan failed: {plan_result.stderr}"

        plan = json.loads(plan_result.stdout)
        assert plan["has_new"] is True, f"plan must find new work: {plan}"
        assert len(plan["versions"]) == 1
        version_entry = plan["versions"][0]
        tag = version_entry["version"]
        # Shape, not today's date: the plan stamps in UTC, so pinning the
        # literal date reds any run that crosses midnight between this line
        # and the plan invocation above.
        assert re.fullmatch(r"1\.0\.0_\d{8}", tag), f"the env plan must stamp the published tag: {version_entry}"
        assert version_entry["source_version"] == "1.0.0", "the source version stays bare"
        assert version_entry["pylock"] is not None, "pypi plan entry must reference its derived lock"

        lock_path = locks_dir / Path(version_entry["pylock"]).name
        assert lock_path.exists(), f"derived lock must be written under locks/: {lock_path}"
        assert "console-pkg" in lock_path.read_text()

        plan_path = tmp_path / "plan.json"
        plan_path.write_text(plan_result.stdout)

        work_dir = tmp_path / "work"
        prepare_result = _run_mirror(
            mirror_binary,
            [
                "package",
                "pipeline",
                "prepare",
                "--spec",
                str(spec_path),
                "--version",
                tag,
                "--plan",
                str(plan_path),
                "--work-dir",
                str(work_dir),
            ],
            env,
        )
        assert prepare_result.returncode == 0, f"prepare failed: {prepare_result.stderr}"

        manifest_path = work_dir / tag / "env-manifest.json"
        assert manifest_path.exists(), "prepare must write env-manifest.json"
        manifest = json.loads(manifest_path.read_text())
        assert manifest["version"] == tag, "push reads the published tag off this field"
        assert len(manifest["envs"]) == 1
        env_entry = manifest["envs"][0]
        assert env_entry["platform"] == "linux/amd64"
        assert len(env_entry["layers"]) == 1

        metadata_path = (work_dir / tag / env_entry["metadata_path"]).resolve()
        assert metadata_path.exists(), f"composed metadata.json must exist: {metadata_path}"

        # Console-script launchers dispatch `python`, never `python3`:
        # python-build-standalone ships `python`/`pythonw` on Windows only, so
        # a `python3` dispatch falls through to the WindowsApps store-alias
        # stub there and hangs. Asserted on the WIRE (the composed
        # metadata.json the push leg hands to `ocx package push -m`), not just
        # in `ocx_python`'s unit tests.
        entrypoints = json.loads(metadata_path.read_text())["entrypoints"]
        assert "console-pkg" in entrypoints, f"the console script must synthesize: {entrypoints}"
        assert entrypoints["console-pkg"]["command"] == "python", entrypoints
        # And the composed env always also carries `python` itself. Without it a
        # bare `python` in a spec's test script resolves to the HOST interpreter
        # under `ocx package test` (or fails to spawn on an image that has none).
        # It carries no `command`, so `ocx launcher exec` resolves the name on
        # the self-view PATH -- the private interpreter's bin/, never itself.
        assert entrypoints.get("python") == {}, f"the composed env must ship a `python` entrypoint: {entrypoints}"
        layer_path = (work_dir / tag / env_entry["layers"][0]["path"]).resolve()
        assert layer_path.exists(), f"repacked wheel layer must exist: {layer_path}"
    finally:
        server.shutdown()


# ---------------------------------------------------------------------------
# Live PyPI: gated, opt-in only
# ---------------------------------------------------------------------------


@pytest.mark.skipif(os.environ.get("OCX_TESTS_ONLINE") != "1", reason="requires OCX_TESTS_ONLINE=1 and network access")
def test_plan_discovers_versions_from_real_pypi(mirror_binary: Path, stub_registry: str, tmp_path: Path) -> None:
    """`pipeline plan` discovers real upstream versions from pypi.org.

    Lock derivation stays stubbed (no real `uv`/interpreter needed) — this
    only exercises `source::pypi::list_versions` against the live JSON API.
    `pycowsay` is a tiny, stable, low-churn package already used throughout
    this codebase's own fixtures.
    """
    package = "pycowsay"
    interpreter_package = "ocx.sh/python/cpython:3.13.1"
    ocx_stub = _write_ocx_pull_stub(tmp_path, interpreter_package)
    # The canned lock's own package name is irrelevant to which upstream
    # version gets selected (discovery is independent of lock content) — see
    # `source::pypi::list_versions` vs `ocx_python::select_wheels`.
    uv_stub = _write_uv_stub(
        tmp_path,
        "uv-online.sh",
        _canned_pylock(package, "1.0.0", "pycowsay-1.0.0-py3-none-any.whl", "https://example.com/pycowsay.whl", "a" * 64),
        0,
    )

    spec_path = _write_spec(
        tmp_path,
        registry=stub_registry,
        repository="pypi-online",
        package=package,
        index="https://pypi.org",
        interpreter_package=interpreter_package,
    )
    result = _run_mirror(
        mirror_binary,
        ["package", "pipeline", "plan", "--spec", str(spec_path), "--locks-dir", str(tmp_path / "locks"), "--format", "json"],
        {
            "OCX_INSECURE_REGISTRIES": stub_registry,
            "OCX_BINARY_PIN": str(ocx_stub),
            "OCX_MIRROR_UV": str(uv_stub),
        },
    )

    assert result.returncode == 0, f"live plan failed: {result.stderr}"
    plan = json.loads(result.stdout)
    assert plan["has_new"] is True, f"expected at least one real pycowsay release discovered: {plan}"
