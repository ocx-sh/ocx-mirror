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


def start_registry(registry: str) -> None:
    """Start the registry via docker-compose if it is not already running."""
    if registry_is_reachable(registry):
        return

    subprocess.run(
        ["docker", "compose", "-f", str(COMPOSE_FILE), "up", "-d"],
        check=True,
        capture_output=True,
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


def push_stub_ocx_package(ocx_binary: Path, registry: str, ref: str, work_dir: Path) -> None:
    """Pushes a minimal one-layer Bundle package to ``{registry}/{ref}`` via
    the real ``ocx`` binary.

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
    metadata_path.write_text(json.dumps({"type": "bundle", "version": 1}))

    layer_path = work_dir / "stub-layer.tar.gz"
    with tarfile.open(layer_path, "w:gz") as tar:
        info = tarfile.TarInfo(name="bin/marker")
        info.size = len(b"stub")
        tar.addfile(info, io.BytesIO(b"stub"))

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
