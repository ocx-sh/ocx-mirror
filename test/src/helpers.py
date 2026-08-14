"""Docker-compose registry helpers for the mirror acceptance-test suite."""
from __future__ import annotations

import io
import json
import os
import subprocess
import tarfile
import time
import urllib.error
import urllib.request
from pathlib import Path

# ---------------------------------------------------------------------------
# Paths
# ---------------------------------------------------------------------------

PROJECT_ROOT = Path(__file__).resolve().parent.parent.parent
COMPOSE_FILE = Path(__file__).resolve().parent.parent / "docker-compose.yml"

# ---------------------------------------------------------------------------
# Docker-compose helpers
# ---------------------------------------------------------------------------


def registry_is_reachable(registry: str) -> bool:
    """Return True if the registry responds to ``GET /v2/``."""
    try:
        urllib.request.urlopen(f"http://{registry}/v2/", timeout=2)
        return True
    except (urllib.error.URLError, OSError):
        return False


def start_registry(registry: str, service: str = "registry") -> None:
    """Start one compose service if the registry it serves is not already up.

    ``service`` is named rather than defaulted to the whole project because
    the two registries can be up independently. GitHub Actions supplies the
    source registry as a job *service container* bound to host 5001, so a
    bare ``docker compose up -d`` — which starts every service — fails to
    bind that port and exits 1, taking the whole session down before a test
    runs. Bringing up only the service that owns the address asked for works
    whether the other one is already bound, bound by something else, or not
    running at all.
    """
    if registry_is_reachable(registry):
        return

    result = subprocess.run(
        ["docker", "compose", "-f", str(COMPOSE_FILE), "up", "-d", service],
        capture_output=True,
        text=True,
    )
    # Surfaced rather than left to `check=True`: a bare CalledProcessError
    # reports the argv and swallows the stderr that says *why*, which is the
    # only part that identifies a port clash.
    if result.returncode != 0:
        raise RuntimeError(
            f"docker compose up -d {service} failed with exit {result.returncode}\n"
            f"stdout: {result.stdout.strip()}\nstderr: {result.stderr.strip()}"
        )

    # Wait for the registry to become reachable (up to 15 s).
    for _ in range(30):
        if registry_is_reachable(registry):
            return
        time.sleep(0.5)

    raise RuntimeError(f"Registry at {registry} did not become reachable")


# ---------------------------------------------------------------------------
# Real-`ocx` package push helper (W3: pypi/mount acceptance suites)
# ---------------------------------------------------------------------------


def push_stub_ocx_package(
    ocx_binary: Path,
    registry: str,
    ref: str,
    work_dir: Path,
    *,
    content: bytes = b"stub",
) -> None:
    """Pushes a minimal one-layer Bundle package to ``{registry}/{ref}`` via
    the real ``ocx`` binary.

    ``content`` is the layer's only file. It is a parameter because the layer
    is otherwise byte-identical between calls, so two versions of the same
    package would land on one manifest digest — and a cascade scenario
    (S-009) needs `1.2` and `latest` to resolve to *different* digests for
    the assertion to be able to fail.

    Used to stand in for a private interpreter package: `ocx-mirror`'s
    in-process interpreter-digest resolution (``fetch_manifest_digest``)
    talks to the registry directly (not via a subprocess), so it needs a
    real manifest to resolve — unlike `materialize_interpreter`'s own
    `OCX_BINARY_PIN`-stubbed `ocx package pull`, which is a separate,
    file-system-only fake that never touches the registry. Content is a
    throwaway marker file; nothing downstream executes it.
    """
    work_dir.mkdir(parents=True, exist_ok=True)
    metadata_path = work_dir / "stub-metadata.json"
    # No `platform` key: ocx >= 0.5.6 retired it from the sidecar — the
    # platform travels solely on the explicit `-p` flag passed below.
    metadata_path.write_text(json.dumps({"type": "bundle", "version": 1}))

    layer_path = work_dir / "stub-layer.tar.gz"
    with tarfile.open(layer_path, "w:gz") as tar:
        info = tarfile.TarInfo(name="bin/marker")
        info.size = len(content)
        tar.addfile(info, io.BytesIO(content))

    env = {
        "PATH": os.environ.get("PATH", ""),
        "OCX_INSECURE_REGISTRIES": registry,
        "OCX_HOME": str(work_dir / "ocx-home"),
    }
    result = subprocess.run(
        [
            str(ocx_binary),
            "--format",
            "json",
            "package",
            "push",
            "-p",
            "linux/amd64",
            "-i",
            f"{registry}/{ref}",
            "-m",
            str(metadata_path),
            str(layer_path),
        ],
        capture_output=True,
        text=True,
        env=env,
    )
    assert result.returncode == 0, f"failed to push stub package {ref} to {registry}: {result.stderr}"


def push_ocx_description(
    ocx_binary: Path,
    registry: str,
    repository: str,
    work_dir: Path,
    *,
    readme: str = "# stub\n",
) -> None:
    """Pushes a `__ocx.desc` description to ``{registry}/{repository}`` via the real ``ocx`` binary.

    The reserved description tag never appears in a root's ``tags{}`` (ocx
    filters reserved tags at render), so it is fetched by name per package —
    which is what S-012 checks travelled.
    """
    work_dir.mkdir(parents=True, exist_ok=True)
    readme_path = work_dir / "README.md"
    readme_path.write_text(readme)

    env = {
        "PATH": os.environ.get("PATH", ""),
        "OCX_INSECURE_REGISTRIES": registry,
        "OCX_HOME": str(work_dir / "ocx-home"),
    }
    result = subprocess.run(
        [
            str(ocx_binary),
            "package",
            "describe",
            f"{registry}/{repository}",
            "--readme",
            str(readme_path),
        ],
        capture_output=True,
        text=True,
        env=env,
    )
    assert result.returncode == 0, f"failed to describe {repository} on {registry}: {result.stderr}"


def fetch_manifest(registry: str, repository: str, reference: str) -> tuple[str, bytes]:
    """GETs `repository:reference`'s manifest from `registry` over the raw OCI HTTP API.

    Returns the digest the registry claims (`Docker-Content-Digest`) and the
    exact response bytes — the verbatim form a published-shape index tree's
    dispatch object must carry (`src.static_index.write_published_index_tree`).
    """
    request = urllib.request.Request(f"http://{registry}/v2/{repository}/manifests/{reference}")
    request.add_header(
        "Accept",
        "application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json",
    )
    with urllib.request.urlopen(request) as response:
        return response.headers["Docker-Content-Digest"], response.read()


def put_manifest(registry: str, repository: str, reference: str, body: bytes, media_type: str) -> str:
    """PUTs a raw manifest to `repository:reference` on `registry`, bypassing `ocx` entirely.

    The only way to seed a descriptor `ocx`'s own writers never produce — an
    attestation/referrer child with no `platform` key (S-024). There is no
    `oras`/`crane` dependency to reach for instead, and `push_stub_ocx_package`
    goes through the real `ocx` binary, which never omits `platform`.

    A malformed `body` surfaces as `urllib.error.HTTPError` from `urlopen`
    itself (the registry answers 400) — no extra handling needed here, same
    as `fetch_manifest`.

    Returns the digest the registry claims for the pushed content
    (`Docker-Content-Digest`).
    """
    request = urllib.request.Request(
        f"http://{registry}/v2/{repository}/manifests/{reference}",
        data=body,
        method="PUT",
        headers={"Content-Type": media_type},
    )
    with urllib.request.urlopen(request) as response:
        return response.headers["Docker-Content-Digest"]
