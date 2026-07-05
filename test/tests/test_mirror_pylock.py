"""W2.6: acceptance tests for the `source.type: pylock` pipeline.

Phase A (negative cases): a malformed/unmirrorable `pylock.toml` must fail
`pipeline plan` with exit 65 (DataError) and an actionable message naming the
offending package. Both cases are registry-free — a tiny local HTTP stub
answers every request with an OCI "repository not found" 404, so `pipeline
plan`'s target-registry read (which runs *before* the pylock read) takes the
fail-safe "first publish" branch instead of needing a real OCI registry.
Run under ``OCX_TESTS_NO_REGISTRY=1`` so conftest.py's session hook never
starts the Docker registry either — these tests never touch it.

Phase B (a real plan -> prepare -> push e2e against :5000) is deferred; see
the skip below.
"""
from __future__ import annotations

import http.server
import json
import os
import subprocess
import sys
import threading
from pathlib import Path

import pytest

FIXTURES_DIR = Path(__file__).resolve().parent.parent / "fixtures" / "mirror-pylock-negative"


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


@pytest.fixture(scope="session")
def mirror_binary() -> Path:
    """Path to the compiled ocx-mirror binary (same resolution as test_mirror_pipeline.py)."""
    if env_path := os.environ.get("OCX_MIRROR_COMMAND"):
        p = Path(env_path)
    else:
        from src.helpers import PROJECT_ROOT
        p = PROJECT_ROOT / "test" / "bin" / "ocx-mirror"
        if sys.platform == "win32" and not p.suffix:
            p = p.with_suffix(".exe")
    if not p.exists():
        pytest.skip(f"ocx-mirror binary not found at {p} — skipping pylock pipeline tests")
    return p


class _RepositoryNotFoundHandler(http.server.BaseHTTPRequestHandler):
    """Minimal OCI-registry stub: every request 404s as NAME_UNKNOWN.

    `pipeline plan` reads the target registry before the source. The
    fail-safe path in `target_registry.rs` treats an authoritative 404 as
    "nothing published yet" (first publish of a new mirror) rather than
    aborting — so this one blanket response is enough to get past that read
    and reach the pylock-source error under test, without a real registry.
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
        pass  # suppress request logging in test output


@pytest.fixture()
def stub_registry() -> str:
    """Starts the local 404-everything OCI stub. Yields its ``host:port``."""
    server = http.server.HTTPServer(("127.0.0.1", 0), _RepositoryNotFoundHandler)
    port = server.server_address[1]
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    yield f"127.0.0.1:{port}"
    server.shutdown()


def _write_spec(tmp_path: Path, fixture_name: str, stub_registry: str) -> Path:
    """Copies a fixture pair (``mirror-<name>.yml`` + ``pylock-<name>.toml``)
    into ``tmp_path``, substituting the registry placeholder."""
    template = (FIXTURES_DIR / f"mirror-{fixture_name}.yml").read_text()
    spec_path = tmp_path / "mirror.yml"
    spec_path.write_text(template.replace("STUB_REGISTRY", stub_registry))
    lock_src = FIXTURES_DIR / f"pylock-{fixture_name}.toml"
    (tmp_path / lock_src.name).write_text(lock_src.read_text())
    return spec_path


def _run_plan(mirror_binary: Path, spec_path: Path, stub_registry: str) -> subprocess.CompletedProcess[str]:
    """Runs ``pipeline plan`` against the stub registry (plain HTTP, no auth)."""
    env = {**os.environ, "OCX_INSECURE_REGISTRIES": stub_registry}
    return subprocess.run(
        [str(mirror_binary), "package", "pipeline", "plan", "--spec", str(spec_path)],
        capture_output=True,
        text=True,
        env=env,
    )


# ---------------------------------------------------------------------------
# Phase A: negative cases — malformed lock content must exit 65 (DataError)
# ---------------------------------------------------------------------------


def test_pylock_plan_rejects_sdist_only_package(mirror_binary: Path, stub_registry: str, tmp_path: Path) -> None:
    """uwsgi-shaped: a package with zero wheels fails at parse (LockError::SdistOnly).

    Regression for the W2.6 error-mapping fix: before it, a pylock load/parse
    failure surfaced from `list_upstream_versions`/`build_pylock_plan_entries`
    was blanket-mapped to `MirrorError::SourceError` (exit 69, "source
    unreachable") even when the failure was malformed lock CONTENT, not an
    unreachable file. Must classify as `PylockError` (exit 65).
    """
    spec_path = _write_spec(tmp_path, "uwsgi", stub_registry)
    result = _run_plan(mirror_binary, spec_path, stub_registry)

    assert result.returncode == 65, (
        f"expected exit 65 (DataError) for a sdist-only lock, got {result.returncode}\nstderr: {result.stderr}"
    )
    assert "uwsgi" in result.stderr, f"error must name the offending package: {result.stderr}"


def test_pylock_plan_rejects_no_wheel_for_target_platform(
    mirror_binary: Path, stub_registry: str, tmp_path: Path
) -> None:
    """psycopg2-shaped: wheels exist but none match the declared linux/amd64 target.

    Fails during wheel selection (`select_wheels` -> `SelectError::NoCompatibleWheel`),
    already mapped to `PylockError` (exit 65) before this fix — this test locks
    in that the message names both the package and the target triple, so it's
    distinguishable from the no-wheel-anywhere case above.
    """
    spec_path = _write_spec(tmp_path, "psycopg2", stub_registry)
    result = _run_plan(mirror_binary, spec_path, stub_registry)

    assert result.returncode == 65, (
        f"expected exit 65 (DataError) for a no-wheel-for-target lock, got {result.returncode}\n"
        f"stderr: {result.stderr}"
    )
    assert "psycopg2" in result.stderr, f"error must name the offending package: {result.stderr}"
    assert "linux/amd64" in result.stderr, f"error must name the target triple: {result.stderr}"


# ---------------------------------------------------------------------------
# Phase B: positive e2e — deferred
# ---------------------------------------------------------------------------


@pytest.mark.skip(
    reason="e2e needs a real cpython on the registry — validated in W3 against dev.ocx.sh"
)
def test_pylock_pipeline_pushes_env_to_registry() -> None:
    """Full plan -> prepare -> push against :5000, asserting the env tag lands.

    Deferred: `prepare` resolves the interpreter package's digest via
    `fetch_manifest_digest` against the registry (`build_interpreter_dependency`
    in `pipeline/prepare.rs`). A hermetic stand-in would need enough real OCX
    package structure (metadata.json + at least one layer, pushed via
    `ocx package push`) to survive that digest fetch — not a small enough
    addition to fake safely here without also faking the very digest-pinning
    behavior the test would be trying to verify. See W3.2 (pycowsay live)
    for the real green loop against dev.ocx.sh.
    """
