"""`ocx-mirror version` against the test build.

Every binary this suite runs is a test build — cargo `--features __testing`
(`task rust:build`, the harness build) or the Bazel `//:ocx-mirror`, which
reads the same `testing_provenance.env`. Its provenance is therefore the fixed
placeholder set, never the commit or CI run it was built from: that is what
keeps the binary, and every cache key hashed over it, stable across commits.
These cases pin the placeholders so a real value leaking back in reds here.

No registry: the runner is built by hand (see `test_mirror_extra_ca.py`).
"""
from __future__ import annotations

import json
import re
from pathlib import Path

import pytest

from src.mirror_runner import MirrorRunner

EPOCH = "1970-01-01T00:00:00.000000000Z"
ZERO_SHA = "0" * 40
SEMVER = re.compile(r"^\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.+-]+)?$")


@pytest.fixture()
def runner(mirror_binary: Path, tmp_path: Path) -> MirrorRunner:
    return MirrorRunner(mirror_binary, "127.0.0.1:1", tmp_path)


def test_plain_prints_the_bare_version_token(runner: MirrorRunner) -> None:
    result = runner.run("version", check=False)
    assert result.returncode == 0, result.stderr
    assert SEMVER.match(result.stdout.strip()), repr(result.stdout)
    assert result.stdout.count("\n") == 1, repr(result.stdout)


def test_json_carries_the_placeholder_provenance(runner: MirrorRunner) -> None:
    result = runner.run("version", "--format", "json", check=False)
    assert result.returncode == 0, result.stderr
    report = json.loads(result.stdout)

    assert set(report) <= {"version", "cargo_pkg_version", "channel", "commit", "build", "ci"}, report
    assert SEMVER.match(report["version"]), report
    assert report["channel"] == "test"
    assert report["commit"] == {
        "sha": ZERO_SHA,
        "short": "00000000",
        "describe": "placeholder-g00000000",
        "dirty": True,
        "timestamp": EPOCH,
    }
    assert report["ci"] == {
        "provider": "github-actions",
        "run_url": "https://ci.invalid/placeholder/placeholder/actions/runs/0",
        "workflow": "placeholder",
        "ref": "refs/heads/placeholder",
        "sha": ZERO_SHA,
    }
    # Only a cargo build bakes toolchain metadata; Bazel runs no build script.
    if "build" in report:
        assert report["build"]["timestamp"] == EPOCH, report["build"]


def test_verbose_json_is_the_same_document(runner: MirrorRunner) -> None:
    plain = runner.run("version", "--format", "json", check=False)
    verbose = runner.run("version", "--verbose", "--format", "json", check=False)
    assert plain.returncode == verbose.returncode == 0, verbose.stderr
    assert json.loads(verbose.stdout) == json.loads(plain.stdout)


def test_verbose_plain_names_the_placeholder_commit_and_channel(runner: MirrorRunner) -> None:
    result = runner.run("version", "--verbose", "--color", "never", check=False)
    assert result.returncode == 0, result.stderr
    lines = result.stdout.splitlines()
    assert re.match(r"^ocx-mirror \S+ \(channel: test\)$", lines[0]), lines
    assert f"commit:   00000000 (dirty) - {EPOCH}" in lines, lines
    assert "ci:       https://ci.invalid/placeholder/placeholder/actions/runs/0" in lines, lines


def test_an_unknown_flag_is_a_usage_error(runner: MirrorRunner) -> None:
    result = runner.run("version", "--bogus", check=False)
    assert result.returncode == 2, (result.returncode, result.stderr)
    assert "--bogus" in result.stderr
