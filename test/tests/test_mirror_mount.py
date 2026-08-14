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
not understand — and the real `registry` fixture.

Uses `source.type: pylock` (a committed lock), not `pypi`: the mount
mechanics under test are identical for both (push dispatches on
`Source::is_env()`), and a committed lock keeps the suite hermetic — no
`uv` lock derivation between prepare and the pushes being asserted on.

The spec pins no `build_timestamp`, so the suite runs the DEFAULT `datetime`
stamp — the shape every real env mirror publishes under. Each version is
therefore addressed by a `X.Y.Z_<14 digits>` tag that only `prepare` knows;
it is read back off `prepare`'s stdout (the manifest path it prints) rather
than recomputed here, since a clock-derived stamp cannot be predicted.
"""
from __future__ import annotations

import hashlib
import http.server
import json
import os
import re
import subprocess
import threading
import urllib.request
import uuid
from pathlib import Path

import pytest

from src.helpers import PROJECT_ROOT, push_stub_ocx_package

FIXTURE_WHEEL = PROJECT_ROOT / "crates" / "ocx_python" / "tests" / "fixtures" / "wheels" / "console_pkg-1.0.0-py3-none-any.whl"


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

wheels:
  linux/amd64: ~

platforms:
  linux/amd64:
    runner: ubuntu-latest
"""
    )


def _write_junit(junit_dir: Path, version: str) -> None:
    """Writes the passing JUnit `push` gates on. `version` is the PUBLISHED (stamped) tag."""
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

        # Bare source version → the stamped tag `prepare` published it under.
        stamped: dict[str, str] = {}

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

            # `prepare` prints the manifest path it wrote; its parent directory
            # IS the published tag, so the run's stamp is read rather than
            # recomputed from a clock this process does not share.
            manifest_path = Path(prepare_result.stdout.strip().splitlines()[-1])
            assert manifest_path.name == "env-manifest.json", f"unexpected prepare stdout: {prepare_result.stdout}"
            assert manifest_path.exists()
            tag = manifest_path.parent.name
            assert re.fullmatch(rf"{re.escape(version)}_\d{{14}}", tag), (
                f"the default `datetime` build stamp must tag {version} as X.Y.Z_<14 digits>, got {tag}"
            )
            stamped[version] = tag

            # `push` keys JUnit lookup off the PUBLISHED tag, not the bare
            # source version — a bare filename here reads as `missing_junit`.
            _write_junit(junit_dir, tag)

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
    assert set(versions) == set(stamped.values()), (
        f"the summary is keyed by the stamped publish tags {sorted(stamped.values())}: {sorted(versions)}"
    )
    for source_version, tag in stamped.items():
        entry = versions[tag]
        assert entry["status"] == "published", f"{tag} must fully publish: {entry}"
        reuse = entry["layer_reuse"]
        assert reuse["mounted"] + reuse["uploaded"] == 1, f"{tag}: exactly one wheel layer, got {reuse}"

        # The stamped tag is the primary one; the rolling aliases it advances
        # are derived from the BARE release, which is what consumers pin.
        cascade = entry["cascade_tags_written"]
        assert cascade[0] == tag, f"{tag}: the stamped tag leads its own cascade set: {cascade}"
        major, minor, _ = source_version.split(".")
        assert {source_version, f"{major}.{minor}", major} <= set(cascade), (
            f"{tag}: the bare cascade tags must be written too: {cascade}"
        )

    # The wheel repo tag must exist on the registry after the first push —
    # read the exact repository/tag `prepare` recorded rather than
    # recomputing ocx_python's wheel-reference naming convention here.
    manifest = json.loads((bundles_dir / stamped["1.0.0"] / "env-manifest.json").read_text())
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
    second = versions[stamped["2.0.0"]]
    assert second["layer_reuse"]["mounted"] > 0, (
        "second push must cross-repository MOUNT the shared wheel layer "
        f"(this registry supports real mounts): {second['layer_reuse']}"
    )
