# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 The OCX Authors
"""`release_sbom.py`'s proofs, against a stub `cargo` on PATH."""

from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

import pytest

SCRIPT = Path(__file__).absolute().parent / "release_sbom.py"
TARGET = "x86_64-unknown-linux-musl"

# Records argv; writes the SBOM beside the manifest unless STUB_FAIL / STUB_NOFILE says otherwise.
STUB = """#!/usr/bin/env bash
printf '%s\\n' "$*" > "$STUB_LOG"
[[ -z "${STUB_FAIL:-}" ]] || exit "$STUB_FAIL"
[[ -z "${STUB_NOFILE:-}" ]] || exit 0
while (($#)); do
    case "$1" in
        --manifest-path) dir="$(dirname "$2")" ;;
        --override-filename) stem="$2" ;;
    esac
    shift
done
echo '{"bomFormat":"CycloneDX"}' > "$dir/$stem.cdx.json"
"""


@pytest.fixture
def run(tmp_path: Path):
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    stub = bin_dir / "cargo"
    stub.write_text(STUB, encoding="utf-8")
    stub.chmod(0o755)
    repo = tmp_path / "repo"
    repo.mkdir()
    log = tmp_path / "argv.log"

    def go(**extra: str) -> tuple[subprocess.CompletedProcess[str], Path, str]:
        out = tmp_path / "out" / "sbom.json"
        result = subprocess.run(
            [
                sys.executable,
                str(SCRIPT),
                "--target",
                TARGET,
                "--output",
                str(out),
                "--manifest-path",
                str(repo / "Cargo.toml"),
            ],
            capture_output=True,
            text=True,
            encoding="utf-8",
            env={
                "PATH": f"{bin_dir}{os.pathsep}{os.environ['PATH']}",
                "STUB_LOG": str(log),
                **extra,
            },
            check=False,
        )
        argv = log.read_text(encoding="utf-8").strip() if log.exists() else ""
        return result, out, argv

    return go


def test_cargo_cyclonedx_runs_for_the_triple_and_the_json_lands_at_output(run) -> None:
    result, out, argv = run()
    assert result.returncode == 0, result.stderr
    assert argv.startswith(f"cyclonedx --format json --target {TARGET} --manifest-path ")
    assert out.read_text(encoding="utf-8").strip() == '{"bomFormat":"CycloneDX"}'


def test_a_cargo_failure_names_the_cause_and_writes_nothing(run) -> None:
    result, out, _ = run(STUB_FAIL="3")
    assert result.returncode == 1
    assert "cargo cyclonedx exited 3" in result.stderr
    assert not out.exists()


def test_no_produced_file_is_a_failure(run) -> None:
    result, out, _ = run(STUB_NOFILE="1")
    assert result.returncode == 1
    assert "wrote no" in result.stderr
    assert not out.exists()


if __name__ == "__main__":
    # The `py_test` entry point (scripts/BUILD.bazel); `pytest scripts` never
    # takes this branch.
    raise SystemExit(pytest.main([__file__, "-q", "-p", "no:cacheprovider"]))
