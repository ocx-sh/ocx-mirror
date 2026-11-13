# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 The OCX Authors
"""`release_sbom.py`'s proofs, against a stub `cargo` on PATH."""

from __future__ import annotations

import os
import shutil
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
# cargo-cyclonedx 0.5.9 appends the format extension to --override-filename.
echo '{"bomFormat":"CycloneDX"}' > "$dir/$stem.json"
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


def test_the_real_locked_cargo_cyclonedx_writes_the_name_the_script_expects(
    tmp_path: Path,
) -> None:
    """The stubs encode an assumption; this drives the pinned tool itself."""
    project = SCRIPT.parent.parent / "packaging" / "release-tools"
    ocx, cargo = shutil.which("ocx"), shutil.which("cargo")
    if ocx is None or cargo is None:
        pytest.skip(f"probed PATH for ocx={ocx} cargo={cargo}; both are needed")
    tool = [ocx, "--project", str(project), "exec", "--"]
    probe = subprocess.run(
        [*tool, "cargo", "cyclonedx", "--version"], capture_output=True, check=False
    )
    if probe.returncode != 0:
        pytest.skip(
            f"probed `ocx --project {project} exec -- cargo cyclonedx --version`: exit {probe.returncode}"
        )
    crate = tmp_path / "crate"
    (crate / "src").mkdir(parents=True)
    (crate / "src" / "main.rs").write_text("fn main() {}\n", encoding="utf-8")
    (crate / "Cargo.toml").write_text(
        '[package]\nname = "x"\nversion = "0.1.0"\nedition = "2021"\n', encoding="utf-8"
    )
    out = tmp_path / "out" / "sbom.json"
    result = subprocess.run(
        [
            *tool,
            sys.executable,
            str(SCRIPT),
            "--target",
            TARGET,
            "--output",
            str(out),
            "--manifest-path",
            str(crate / "Cargo.toml"),
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 0, result.stderr
    assert '"bomFormat": "CycloneDX"' in out.read_text(encoding="utf-8")


if __name__ == "__main__":
    # The `py_test` entry point (scripts/BUILD.bazel); `pytest scripts` never
    # takes this branch.
    raise SystemExit(pytest.main([__file__, "-q", "-p", "no:cacheprovider"]))
