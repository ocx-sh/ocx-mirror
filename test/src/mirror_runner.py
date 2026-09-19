from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path
from typing import Any

from src.helpers import PROJECT_ROOT, zot_registry_address


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
        # The `ocx` the mirror spawns is the `ocx` the harness drives: the
        # mirror resolves `OCX_BINARY_PIN` before PATH, so whatever
        # `OCX_COMMAND` names (the taskfile resolves it, CI points it at the
        # submodule build) is what `pipeline push` / `cascade` / `announce`
        # run — not an older toolchain `ocx` that happens to sit on PATH and
        # rejects a flag the pinned ocx already knows. Resolved to an
        # absolute path, because the child runs out of `temp_dir` and a
        # relative `OCX_COMMAND` would dangle there; an empty variable pins
        # the same `test/bin/ocx` conftest's `ocx_binary` fixture falls back
        # to, so the harness's ocx and the mirror's child are one binary.
        ocx_command = Path(os.environ.get("OCX_COMMAND") or PROJECT_ROOT / "test" / "bin" / "ocx")
        if sys.platform == "win32" and not ocx_command.suffix:
            ocx_command = ocx_command.with_suffix(".exe")
        self.env["OCX_BINARY_PIN"] = str(ocx_command.resolve())
        # Mirror-signing harness (WP 5, C-073): the mirror resolves `sign:`
        # refs itself (adr_mirror_signing.md D1), so each variable a signing
        # fixture names under `env://` must be on this constructed whitelist
        # explicitly or the mirror process never sees it -- plugin-dispatch
        # scrub or not (ADR F2). Forwarded only when actually set in the
        # parent environment: a blank default would shadow a variable the
        # subprocess's own defaulting logic is supposed to see as absent.
        # `OCX_EXTRA_CA_CERTS` rides the same whitelist: the mirror's startup
        # gate resolves it before dispatch, and the extra-CA acceptance test
        # sets it to observe that gate's exit code.
        for name in (
            "SIGSTORE_FULCIO_URL",
            "SIGSTORE_REKOR_URL",
            "MIRROR_SIGNING_KEY",
            "MIRROR_KEY_PASSPHRASE",
            "OCX_CONFIG",
            "OCX_EXTRA_CA_CERTS",
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
