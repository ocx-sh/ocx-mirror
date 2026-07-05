"""W3: acceptance test for shared-wheel-layer reuse across env-package pushes.

Two mirror "app versions" (1.0.0, 2.0.0) lock the exact same wheel (the
`console_pkg` fixture wheel, reused unchanged by `ocx_python`'s own tests) —
a stand-in for an app bump that doesn't touch a shared dependency. Both
`prepare` legs produce a wheel layer with the identical `wheel_sha256`; the
push driver (`pipeline::python_push::register_wheel_layers` +
`build_env_push_args`'s `:from=` mount tail, Decision D — shared wheel
layers) is expected to push that wheel's standalone
`pip-packages/...:<sha256>` package once and cross-repository *mount* it
for every subsequent leg that needs the same blob, instead of re-uploading.

This exercises the pipeline against the REAL `ocx` binary built from the
`external/ocx` submodule pin (`real_ocx_binary`, conftest.py) — the mount
tail syntax and the JSON `layers` push-report field are recent additions
that an older `ocx` resolved from PATH/OCX_COMMAND in this environment would
not understand — and the real `:5000` registry fixture.

Uses `source.type: pylock` (a committed lock), not `pypi`: at time of
writing, `pipeline push`'s env-vs-archive dispatch
(`command/package/pipeline/push.rs::Push::execute`) special-cases only
`Source::Pylock`, not `Source::Pypi`, even though `Source::is_env()` (used
correctly by `prepare.rs`) covers both — see the deviation note in this
suite's docstring-adjacent commit message. `pylock` sidesteps that gap
entirely, since it is unconditionally supported by the existing dispatch.
"""
from __future__ import annotations

import hashlib
import http.server
import json
import os
import subprocess
import sys
import threading
import urllib.request
import uuid
from pathlib import Path

import pytest

from src.helpers import PROJECT_ROOT, push_stub_ocx_package

FIXTURE_WHEEL = PROJECT_ROOT / "crates" / "ocx_python" / "tests" / "fixtures" / "wheels" / "console_pkg-1.0.0-py3-none-any.whl"


@pytest.fixture(scope="session")
def mirror_binary() -> Path:
    if env_path := os.environ.get("OCX_MIRROR_COMMAND"):
        p = Path(env_path)
    else:
        p = PROJECT_ROOT / "test" / "bin" / "ocx-mirror"
        if sys.platform == "win32" and not p.suffix:
            p = p.with_suffix(".exe")
    if not p.exists():
        pytest.skip(f"ocx-mirror binary not found at {p} — skipping mount test")
    return p


def _serve_wheel(wheel_bytes: bytes) -> tuple[str, http.server.HTTPServer]:
    """Serves `wheel_bytes` at `/console_pkg-1.0.0-py3-none-any.whl`. Returns base URL + server."""

    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self) -> None:  # noqa: N802
            self.send_response(200)
            self.send_header("Content-Type", "application/octet-stream")
            self.send_header("Content-Length", str(len(wheel_bytes)))
            self.end_headers()
            self.wfile.write(wheel_bytes)

        def log_message(self, fmt: str, *args: object) -> None:  # noqa: ANN002
            pass

    server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
    port = server.server_address[1]
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return f"http://127.0.0.1:{port}", server


def _write_pylock(path: Path, version: str, wheel_url: str, sha256: str) -> None:
    path.write_text(
        'lock-version = "1.0"\n'
        'requires-python = ">=3.9"\n'
        "\n"
        "[[packages]]\n"
        'name = "acme-mount-app"\n'
        f'version = "{version}"\n'
        "\n"
        "[[packages.wheels]]\n"
        'name = "console_pkg-1.0.0-py3-none-any.whl"\n'
        f'url = "{wheel_url}"\n'
        f'hashes = {{ sha256 = "{sha256}" }}\n'
    )


def _write_spec(path: Path, *, registry: str, repository: str, interpreter_package: str) -> None:
    path.write_text(
        f"""name: acme-mount-app
target:
  registry: {registry}
  repository: {repository}

source:
  type: pylock
  path: pylock.toml

python:
  version: "3.13.1"
  abi: cp313
  interpreter_package: "{interpreter_package}"

platforms:
  linux/amd64:
    runner: ubuntu-latest
"""
    )


def _write_junit(junit_dir: Path, version: str) -> None:
    xml = f"""<?xml version="1.0" encoding="UTF-8"?>
<testsuites>
  <testsuite name="ocx-mirror.acme-mount-app.linux_amd64._native_"
             tests="1" failures="0" errors="0" skipped="0"
             timestamp="2026-07-05T10:00:00Z" time="1.0">
    <properties>
      <property name="ocx.version" value="{version}"/>
      <property name="ocx.platform" value="linux/amd64"/>
      <property name="ocx.image" value="_native_"/>
    </properties>
    <testcase name="version" classname="ocx-mirror.acme-mount-app.linux_amd64._native_" time="1.0"/>
  </testsuite>
</testsuites>"""
    junit_dir.mkdir(parents=True, exist_ok=True)
    (junit_dir / f"junit-{version}-linux_amd64-_native_.xml").write_text(xml)


def _run_mirror(mirror_binary: Path, args: list[str], env: dict[str, str]) -> subprocess.CompletedProcess[str]:
    full_env = {**os.environ, **env}
    return subprocess.run([str(mirror_binary), *args], capture_output=True, text=True, env=full_env)


def test_shared_wheel_layer_mounted_on_second_push(
    mirror_binary: Path,
    real_ocx_binary: Path,
    registry: str,
    tmp_path: Path,
) -> None:
    unique = uuid.uuid4().hex[:8]
    repository = f"mount-test-{unique}"

    interpreter_ref = f"python/cpython-mount-{unique}:3.13.1"
    push_stub_ocx_package(real_ocx_binary, registry, interpreter_ref, tmp_path / "push-setup")
    interpreter_package = f"{registry}/{interpreter_ref}"

    wheel_bytes = FIXTURE_WHEEL.read_bytes()
    sha256 = hashlib.sha256(wheel_bytes).hexdigest()
    wheel_index, wheel_server = _serve_wheel(wheel_bytes)
    try:
        wheel_url = f"{wheel_index}/console_pkg-1.0.0-py3-none-any.whl"

        bundles_dir = tmp_path / "bundles"
        junit_dir = tmp_path / "junit"
        env = {"OCX_INSECURE_REGISTRIES": registry}

        for version in ("1.0.0", "2.0.0"):
            version_dir = tmp_path / version
            version_dir.mkdir()
            _write_pylock(version_dir / "pylock.toml", version, wheel_url, sha256)
            spec_path = version_dir / "mirror.yml"
            _write_spec(spec_path, registry=registry, repository=repository, interpreter_package=interpreter_package)

            prepare_result = _run_mirror(
                mirror_binary,
                [
                    "package",
                    "pipeline",
                    "prepare",
                    "--spec",
                    str(spec_path),
                    "--version",
                    version,
                    "--work-dir",
                    str(bundles_dir),
                ],
                env,
            )
            assert prepare_result.returncode == 0, f"prepare {version} failed: {prepare_result.stderr}"
            assert (bundles_dir / version / "env-manifest.json").exists()

            _write_junit(junit_dir, version)

        # Both versions' bundles are pushed via a SINGLE `pipeline push`
        # invocation (its own contract: one serial driver pass enumerates
        # every version under `--bundles-dir`) — either version's spec works
        # here, since `execute_pylock_push` only reads spec.target/platforms,
        # not spec.source, once the Pylock dispatch has been taken.
        summary_path = tmp_path / "run-summary.json"
        push_env = {**env, "OCX_BINARY_PIN": str(real_ocx_binary)}
        push_result = _run_mirror(
            mirror_binary,
            [
                "package",
                "pipeline",
                "push",
                "--spec",
                str(tmp_path / "2.0.0" / "mirror.yml"),
                "--bundles-dir",
                str(bundles_dir),
                "--junit-dir",
                str(junit_dir),
                "--write-summary",
                str(summary_path),
            ],
            push_env,
        )
        assert push_result.returncode == 0, f"push failed: {push_result.stdout}\n{push_result.stderr}"
    finally:
        wheel_server.shutdown()

    summary = json.loads(summary_path.read_text())
    versions = {v["version"]: v for v in summary["versions"]}
    assert set(versions) == {"1.0.0", "2.0.0"}
    for version, entry in versions.items():
        assert entry["status"] == "published", f"{version} must fully publish: {entry}"
        reuse = entry["layer_reuse"]
        assert reuse["mounted"] + reuse["uploaded"] == 1, f"{version}: exactly one wheel layer, got {reuse}"

    # The wheel repo tag must exist on the registry after the first push —
    # read the exact repository/tag `prepare` recorded rather than
    # recomputing ocx_python's wheel-reference naming convention here.
    manifest = json.loads((bundles_dir / "1.0.0" / "env-manifest.json").read_text())
    layer = manifest["envs"][0]["layers"][0]
    assert layer["wheel_sha256"] == sha256
    wheel_repository = layer["wheel_repository"]

    with urllib.request.urlopen(f"http://{registry}/v2/{wheel_repository}/tags/list") as response:
        assert response.status == 200
        tags = json.loads(response.read())["tags"]
    assert sha256 in tags, f"wheel repo tag must exist after the first push: {wheel_repository}:{sha256} not in {tags}"

    # Empirically confirmed against this fixture's `registry:2` image: it
    # DOES honor cross-repository blob mount (`POST .../blobs/uploads/
    # ?mount=<digest>&from=<repo>`), so the second version's push reuses the
    # blob via a real mount rather than falling back to a re-upload.
    assert versions["2.0.0"]["layer_reuse"]["mounted"] > 0, (
        "second push must cross-repository MOUNT the shared wheel layer "
        f"(this registry supports real mounts): {versions['2.0.0']['layer_reuse']}"
    )
