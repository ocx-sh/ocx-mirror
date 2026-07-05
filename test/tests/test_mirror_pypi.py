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
`OCX_BINARY_PIN`. It runs against the real `:5000` registry fixture, with a
throwaway interpreter package pushed there via the real `ocx` binary
(`real_ocx_binary`/`push_stub_ocx_package`, conftest.py) — the content is a
one-byte marker; nothing downstream ever executes it.
"""
from __future__ import annotations

import hashlib
import http.server
import json
import os
import socket
import stat
import subprocess
import sys
import threading
import uuid
from pathlib import Path

import pytest

from src.helpers import PROJECT_ROOT, push_stub_ocx_package

FIXTURE_WHEEL = PROJECT_ROOT / "crates" / "ocx_python" / "tests" / "fixtures" / "wheels" / "console_pkg-1.0.0-py3-none-any.whl"

# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


@pytest.fixture(scope="session")
def mirror_binary() -> Path:
    """Path to the compiled ocx-mirror binary (same resolution as test_mirror_pylock.py)."""
    if env_path := os.environ.get("OCX_MIRROR_COMMAND"):
        p = Path(env_path)
    else:
        p = PROJECT_ROOT / "test" / "bin" / "ocx-mirror"
        if sys.platform == "win32" and not p.suffix:
            p = p.with_suffix(".exe")
    if not p.exists():
        pytest.skip(f"ocx-mirror binary not found at {p} — skipping pypi pipeline tests")
    return p


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


def _make_pypi_handler(projects: dict[str, bytes], wheels: dict[str, bytes]) -> type:
    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self) -> None:  # noqa: N802
            parts = self.path.strip("/").split("/")
            if len(parts) == 3 and parts[0] == "pypi" and parts[2] == "json" and parts[1] in projects:
                self._send(200, projects[parts[1]], "application/json")
                return
            if len(parts) == 3 and parts[0] == "pypi" and parts[2] == "json":
                self._send(404, json.dumps({"message": "Not Found"}).encode(), "application/json")
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


def _start_fake_pypi(projects: dict[str, bytes], wheels: dict[str, bytes]) -> tuple[str, http.server.HTTPServer]:
    """Starts a local stand-in for a PyPI-compatible index. Returns its base URL + server."""
    server = http.server.HTTPServer(("127.0.0.1", 0), _make_pypi_handler(projects, wheels))
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
    return json.dumps({"info": {}, "releases": releases, "urls": [], "vulnerabilities": []}).encode()


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
) -> Path:
    spec = f"""name: {package}
target:
  registry: {registry}
  repository: {repository}

source:
  type: pypi
  package: {package}
  index: {index}

python:
  version: "3.13.1"
  abi: cp313
  interpreter_package: "{interpreter_package}"

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


def test_plan_maps_uv_nonzero_exit_to_data_error(mirror_binary: Path, stub_registry: str, tmp_path: Path) -> None:
    """A `uv pip compile` resolution failure is malformed lock content: PylockError, exit 65."""
    releases = {"1.0.0": [{"filename": "acme-app-1.0.0-py3-none-any.whl", "yanked": False}]}
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
    releases = {"1.0.0": [{"filename": "acme-app-1.0.0-py3-none-any.whl", "yanked": False}]}
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
        assert version_entry["version"] == "1.0.0"
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
                "1.0.0",
                "--plan",
                str(plan_path),
                "--work-dir",
                str(work_dir),
            ],
            env,
        )
        assert prepare_result.returncode == 0, f"prepare failed: {prepare_result.stderr}"

        manifest_path = work_dir / "1.0.0" / "env-manifest.json"
        assert manifest_path.exists(), "prepare must write env-manifest.json"
        manifest = json.loads(manifest_path.read_text())
        assert manifest["version"] == "1.0.0"
        assert len(manifest["envs"]) == 1
        env_entry = manifest["envs"][0]
        assert env_entry["platform"] == "linux/amd64"
        assert len(env_entry["layers"]) == 1

        metadata_path = (work_dir / "1.0.0" / env_entry["metadata_path"]).resolve()
        assert metadata_path.exists(), f"composed metadata.json must exist: {metadata_path}"
        layer_path = (work_dir / "1.0.0" / env_entry["layers"][0]["path"]).resolve()
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
