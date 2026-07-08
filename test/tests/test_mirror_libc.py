"""Acceptance tests for `+libc.*` wheels keys as OCI `os.features` entries.

The `wheels:` map's platform keys are published VERBATIM as image-index
platform entries: a dual-libc spec (`linux/amd64+libc.glibc` +
`linux/amd64+libc.musl`) publishes ONE bare tag whose index carries two
platform entries for the same os/arch, distinguished only by `os.features`;
a featureless key publishes a single entry with no `os.features` at all.

Both tests run the real prepare → push pipeline against the `:5000` registry
fixture with the real `ocx` binary (same setup as test_mirror_mount.py), then
assert on the raw OCI image index fetched straight from the registry.
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

FIXTURE_WHEEL = (
    PROJECT_ROOT / "crates" / "ocx_python" / "tests" / "fixtures" / "wheels" / "console_pkg-1.0.0-py3-none-any.whl"
)


@pytest.fixture(scope="session")
def mirror_binary() -> Path:
    if env_path := os.environ.get("OCX_MIRROR_COMMAND"):
        p = Path(env_path)
    else:
        p = PROJECT_ROOT / "test" / "bin" / "ocx-mirror"
        if sys.platform == "win32" and not p.suffix:
            p = p.with_suffix(".exe")
    if not p.exists():
        pytest.skip(f"ocx-mirror binary not found at {p} — skipping libc os.features tests")
    return p


def _serve_files(files: dict[str, bytes]) -> tuple[str, http.server.HTTPServer]:
    """Serves each `files` entry at `/<name>`. Returns base URL + server."""

    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self) -> None:  # noqa: N802
            name = self.path.lstrip("/")
            body = files.get(name)
            if body is None:
                self.send_response(404)
                self.end_headers()
                return
            self.send_response(200)
            self.send_header("Content-Type", "application/octet-stream")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, fmt: str, *args: object) -> None:  # noqa: ANN002
            pass

    server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
    port = server.server_address[1]
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return f"http://127.0.0.1:{port}", server


def _pylock(wheels: list[tuple[str, str, str]]) -> str:
    """PEP 751 lock for `acme-libc-app` 1.0.0 with the given (filename, url, sha256) wheels."""
    lock = (
        'lock-version = "1.0"\n'
        'requires-python = ">=3.9"\n'
        "\n"
        "[[packages]]\n"
        'name = "acme-libc-app"\n'
        'version = "1.0.0"\n'
    )
    for filename, url, sha256 in wheels:
        lock += (
            "\n[[packages.wheels]]\n"
            f'name = "{filename}"\n'
            f'url = "{url}"\n'
            f'hashes = {{ sha256 = "{sha256}" }}\n'
        )
    return lock


def _junit(junit_dir: Path, version: str, container_id: str) -> None:
    xml = f"""<?xml version="1.0" encoding="UTF-8"?>
<testsuites>
  <testsuite name="acme-libc-app.{version}.linux_amd64.{container_id}"
             tests="1" failures="0" errors="0" skipped="0"
             timestamp="2026-07-09T10:00:00Z" time="1.0">
    <testcase name="version" classname="acme-libc-app.{version}.linux_amd64.{container_id}" time="1.0"/>
  </testsuite>
</testsuites>"""
    junit_dir.mkdir(parents=True, exist_ok=True)
    (junit_dir / f"junit-{version}-linux_amd64-{container_id}.xml").write_text(xml)


def _run_mirror(mirror_binary: Path, args: list[str], env: dict[str, str]) -> subprocess.CompletedProcess[str]:
    full_env = {**os.environ, **env}
    return subprocess.run([str(mirror_binary), *args], capture_output=True, text=True, env=full_env)


def _fetch_index(registry: str, repository: str, tag: str) -> dict:
    request = urllib.request.Request(
        f"http://{registry}/v2/{repository}/manifests/{tag}",
        headers={"Accept": "application/vnd.oci.image.index.v1+json"},
    )
    with urllib.request.urlopen(request) as response:
        assert response.status == 200
        return json.loads(response.read())


def _prepare_and_push(
    mirror_binary: Path,
    real_ocx_binary: Path,
    registry: str,
    tmp_path: Path,
    spec_yaml: str,
    lock_toml: str,
    junit_container_ids: list[str],
) -> dict:
    """Runs prepare + push for version 1.0.0 and returns the run summary."""
    spec_path = tmp_path / "mirror.yml"
    spec_path.write_text(spec_yaml)
    (tmp_path / "pylock.toml").write_text(lock_toml)

    bundles_dir = tmp_path / "bundles"
    junit_dir = tmp_path / "junit"
    env = {"OCX_INSECURE_REGISTRIES": registry}

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
            "--work-dir",
            str(bundles_dir),
        ],
        env,
    )
    assert prepare_result.returncode == 0, f"prepare failed: {prepare_result.stderr}"
    assert (bundles_dir / "1.0.0" / "env-manifest.json").exists()

    for container_id in junit_container_ids:
        _junit(junit_dir, "1.0.0", container_id)

    summary_path = tmp_path / "run-summary.json"
    push_result = _run_mirror(
        mirror_binary,
        [
            "package",
            "pipeline",
            "push",
            "--spec",
            str(spec_path),
            "--bundles-dir",
            str(bundles_dir),
            "--junit-dir",
            str(junit_dir),
            "--write-summary",
            str(summary_path),
        ],
        {**env, "OCX_BINARY_PIN": str(real_ocx_binary)},
    )
    assert push_result.returncode == 0, f"push failed: {push_result.stdout}\n{push_result.stderr}"
    return json.loads(summary_path.read_text())


def test_dual_libc_keys_publish_one_tag_with_two_os_features_entries(
    mirror_binary: Path,
    real_ocx_binary: Path,
    registry: str,
    tmp_path: Path,
) -> None:
    """`+libc.glibc` + `+libc.musl` keys → ONE bare tag, one image index, two
    platform entries for the same os/arch carrying the respective os.features."""
    unique = uuid.uuid4().hex[:8]
    repository = f"libc-dual-{unique}"

    interpreter_ref = f"python/cpython-libc-{unique}:3.13.1"
    push_stub_ocx_package(real_ocx_binary, registry, interpreter_ref, tmp_path / "push-setup")
    interpreter_package = f"{registry}/{interpreter_ref}"

    # Fabricated per-libc wheels: same fixture bytes under a manylinux and a
    # musllinux filename — selection is filename-tag-driven, the content only
    # has to be a real repackable wheel.
    wheel_bytes = FIXTURE_WHEEL.read_bytes()
    sha256 = hashlib.sha256(wheel_bytes).hexdigest()
    glibc_wheel = "console_pkg-1.0.0-py3-none-manylinux_2_28_x86_64.whl"
    musl_wheel = "console_pkg-1.0.0-py3-none-musllinux_1_2_x86_64.whl"
    index_url, server = _serve_files({glibc_wheel: wheel_bytes, musl_wheel: wheel_bytes})
    try:
        spec_yaml = f"""name: acme-libc-app
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
  "linux/amd64+libc.glibc": ~
  "linux/amd64+libc.musl": ~

platforms:
  linux/amd64:
    runner: ubuntu-latest
    containers:
      - image: debian:12
      - image: alpine:3.20
"""
        lock_toml = _pylock(
            [
                (glibc_wheel, f"{index_url}/{glibc_wheel}", sha256),
                (musl_wheel, f"{index_url}/{musl_wheel}", sha256),
            ]
        )
        summary = _prepare_and_push(
            mirror_binary,
            real_ocx_binary,
            registry,
            tmp_path,
            spec_yaml,
            lock_toml,
            junit_container_ids=["debian_12", "alpine_3_20"],
        )
    finally:
        server.shutdown()

    versions = {v["version"]: v for v in summary["versions"]}
    entry = versions["1.0.0"]
    assert entry["status"] == "published", f"both libc entries must publish: {entry}"
    assert sorted(entry["platforms_pushed"]) == [
        "linux/amd64+libc.glibc",
        "linux/amd64+libc.musl",
    ], f"both full keys pushed: {entry['platforms_pushed']}"

    index = _fetch_index(registry, repository, "1.0.0")
    platforms = [m["platform"] for m in index["manifests"]]
    features = sorted(tuple(p.get("os.features", [])) for p in platforms)
    assert all(p["os"] == "linux" and p["architecture"] == "amd64" for p in platforms), platforms
    assert features == [("libc.glibc",), ("libc.musl",)], (
        f"one index, two entries distinguished by os.features: {platforms}"
    )


def test_featureless_key_publishes_single_entry_without_os_features(
    mirror_binary: Path,
    real_ocx_binary: Path,
    registry: str,
    tmp_path: Path,
) -> None:
    """A plain `linux/amd64` key publishes a single index entry with NO
    `os.features` serialized — runnable on glibc AND musl hosts."""
    unique = uuid.uuid4().hex[:8]
    repository = f"libc-plain-{unique}"

    interpreter_ref = f"python/cpython-plain-{unique}:3.13.1"
    push_stub_ocx_package(real_ocx_binary, registry, interpreter_ref, tmp_path / "push-setup")
    interpreter_package = f"{registry}/{interpreter_ref}"

    wheel_bytes = FIXTURE_WHEEL.read_bytes()
    sha256 = hashlib.sha256(wheel_bytes).hexdigest()
    any_wheel = "console_pkg-1.0.0-py3-none-any.whl"
    index_url, server = _serve_files({any_wheel: wheel_bytes})
    try:
        spec_yaml = f"""name: acme-libc-app
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
        lock_toml = _pylock([(any_wheel, f"{index_url}/{any_wheel}", sha256)])
        summary = _prepare_and_push(
            mirror_binary,
            real_ocx_binary,
            registry,
            tmp_path,
            spec_yaml,
            lock_toml,
            junit_container_ids=["_native_"],
        )
    finally:
        server.shutdown()

    versions = {v["version"]: v for v in summary["versions"]}
    assert versions["1.0.0"]["status"] == "published"
    assert versions["1.0.0"]["platforms_pushed"] == ["linux/amd64"]

    index = _fetch_index(registry, repository, "1.0.0")
    assert len(index["manifests"]) == 1, index
    platform = index["manifests"][0]["platform"]
    assert platform["os"] == "linux" and platform["architecture"] == "amd64", platform
    assert "os.features" not in platform, f"featureless key must serialize no os.features: {platform}"
