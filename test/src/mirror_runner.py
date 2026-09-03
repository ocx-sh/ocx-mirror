from __future__ import annotations

import os
import subprocess
from pathlib import Path
from typing import Any

from src.helpers import zot_registry_address


class MirrorRunner:
    """Wraps the ocx-mirror binary with per-test environment isolation."""

    def __init__(self, binary: Path, registry: str, temp_dir: Path):
        self.binary = binary
        self.registry = registry
        self.temp_dir = temp_dir
        self.env: dict[str, str] = {
            # Zot (WP 5, C-073): the signing harness's third registry, so a
            # push into it never needs the corresponding `zot_registry`
            # fixture's caller to also thread it onto every mirror
            # invocation's `--insecure-registries` by hand. The address comes
            # from the same helper the `zot_registry` fixture calls -- two
            # independent reads of ZOT_REGISTRY leave a relocated zot
            # declared insecure at an address nothing serves.
            "OCX_INSECURE_REGISTRIES": ",".join((registry, zot_registry_address())),
            "PATH": os.environ.get("PATH", ""),
            "HOME": os.environ.get("HOME", str(Path.home())),
        }
        # Mirror-signing harness (WP 5, C-073): the mirror resolves `sign:`
        # refs itself (adr_mirror_signing.md D1), so each variable a signing
        # fixture names under `env://` must be on this constructed whitelist
        # explicitly or the mirror process never sees it -- plugin-dispatch
        # scrub or not (ADR F2). Forwarded only when actually set in the
        # parent environment: a blank default would shadow a variable the
        # subprocess's own defaulting logic is supposed to see as absent.
        for name in (
            "SIGSTORE_FULCIO_URL",
            "SIGSTORE_REKOR_URL",
            "MIRROR_SIGNING_KEY",
            "MIRROR_KEY_PASSPHRASE",
            "OCX_CONFIG",
        ):
            if name in os.environ:
                self.env[name] = os.environ[name]

    def run(
        self,
        *args: str,
        check: bool = True,
    ) -> subprocess.CompletedProcess[str]:
        """Run ocx-mirror with the given arguments."""
        cmd = [str(self.binary)] + list(args)
        result = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            env=self.env,
            # Run out of the per-test scratch directory: `pipeline patch` writes
            # its metadata sidecars to a relative `.ocx-mirror/patch-<pid>/`, and
            # inheriting pytest's own cwd would put those in the repository.
            cwd=str(self.temp_dir),
        )
        if check and result.returncode != 0:
            raise AssertionError(
                f"ocx-mirror {' '.join(args)} failed (rc={result.returncode})\n"
                f"stdout: {result.stdout.strip()}\n"
                f"stderr: {result.stderr.strip()}"
            )
        return result
