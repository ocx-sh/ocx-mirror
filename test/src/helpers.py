"""Docker-compose registry helpers for the mirror acceptance-test suite."""
from __future__ import annotations

import subprocess
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
