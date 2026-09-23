"""Telemetry names and the never-fail contract (ADR adr_bazel_crate_split.md A-8, C-013).

Run by `task telemetry:self-test` as `uv run --project test pytest scripts -q`.

* The two JUnit producers — the CI composite action and `telemetry:push` —
  must push under the mirror's own service name, trace-name prefix and repo
  attribute, or the Grafana `repo` variable files mirror spans under ocx.
* Telemetry never decides a job or a task: an unreachable endpoint and a
  failing `junit2otlp` exit 0 with one `telemetry` warning line; no endpoint
  exits 0 in silence.

Scratch lives under `~/.cache/ocx-mirror-pytest-tmp`, outside any git
checkout, and every subprocess gets a throwaway `HOME` so the operator's real
`~/.config/ocx-telemetry/env` is never sourced.
"""

import os
import re
import shutil
import subprocess
import tempfile
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parent.parent
ACTION = REPO_ROOT / ".github" / "actions" / "test-telemetry" / "action.yml"
TASKFILE = REPO_ROOT / "taskfiles" / "telemetry.taskfile.yml"
UNREACHABLE = "https://127.0.0.1:9"

JUNIT = """<?xml version="1.0" encoding="UTF-8"?>
<testsuites><testsuite name="s" tests="1"><testcase classname="c" name="t" time="0.01"/></testsuite></testsuites>
"""


# --- names ------------------------------------------------------------------


# Anchored to the argv / attribute lines themselves: a bare substring match
# also greens on the prose in each file's header that names the same values.
@pytest.mark.parametrize(
    ("path", "trace_name", "attribute"),
    [
        (
            ACTION,
            r'--trace-name "ocx-mirror-\$\{SUITE\}"',
            r'^\s*attributes="\$\{attributes\},vcs\.repository\.name=ocx-mirror"$',
        ),
        (
            TASKFILE,
            r'--trace-name "ocx-mirror-\{\{\.SUITE\}\}"',
            r'^\s*--additional-attributes "[^"\n]*,vcs\.repository\.name=ocx-mirror(,[^"\n]*)?"$',
        ),
    ],
    ids=["action", "taskfile"],
)
def test_junit_producer_carries_mirror_names(path: Path, trace_name: str, attribute: str) -> None:
    text = path.read_text(encoding="utf-8")
    assert re.search(r"^\s*--service-name ocx-mirror-tests( \\)?$", text, re.MULTILINE)
    assert re.search(rf"^\s*{trace_name}( \\)?$", text, re.MULTILINE)
    assert re.search(attribute, text, re.MULTILINE)
    # ocx's own names must be gone, or mirror spans land in ocx's series.
    assert not re.search(r"--service-name ocx-tests\b", text)
    assert '--trace-name "ocx-${' not in text and '--trace-name "ocx-{{' not in text


# --- never-fail -------------------------------------------------------------


@pytest.fixture
def scratch():
    root = Path.home() / ".cache" / "ocx-mirror-pytest-tmp"
    root.mkdir(parents=True, exist_ok=True)
    directory = Path(tempfile.mkdtemp(dir=root))
    (directory / "home").mkdir()
    (directory / "bin").mkdir()
    (directory / "tmp").mkdir()
    (directory / "junit.xml").write_text(JUNIT, encoding="utf-8")
    yield directory
    shutil.rmtree(directory, ignore_errors=True)


def _fake(bin_dir: Path, name: str, body: str) -> None:
    tool = bin_dir / name
    tool.write_text(f"#!/bin/sh\n{body}\n", encoding="utf-8")
    tool.chmod(0o755)


def _env(scratch: Path, **extra: str) -> dict[str, str]:
    env = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith(("OTEL_", "GITHUB_", "RUNNER_"))
    }
    env["HOME"] = str(scratch / "home")
    env["TMPDIR"] = str(scratch / "tmp")
    env["PATH"] = f"{scratch / 'bin'}{os.pathsep}{env.get('PATH', '')}"
    env.update(extra)
    return env


def _push(scratch: Path, env: dict[str, str]) -> subprocess.CompletedProcess[str]:
    task = shutil.which("task", path=env["PATH"])
    assert task, "go-task must be on PATH (run via `task telemetry:self-test`)"
    return subprocess.run(
        [
            task,
            "--taskfile",
            str(TASKFILE),
            "push",
            f"JUNIT={scratch / 'junit.xml'}",
            "SUITE=unit",
        ],
        env=env,
        capture_output=True,
        encoding="utf-8",
        timeout=120,
        check=False,
    )


def test_push_failing_junit2otlp_exits_zero_with_warning(scratch: Path) -> None:
    _fake(scratch / "bin", "junit2otlp", "echo 'dial tcp 127.0.0.1:9: connect: refused' >&2; exit 1")
    result = _push(scratch, _env(scratch, OTEL_EXPORTER_OTLP_ENDPOINT=UNREACHABLE))
    assert result.returncode == 0, result
    assert "telemetry: junit2otlp failed:" in result.stderr, result


def test_push_undelivered_export_exits_zero_with_warning(scratch: Path) -> None:
    # junit2otlp's real shape against a dead endpoint: the export error is
    # logged and the process still exits 0, so only the log carries it.
    _fake(
        scratch / "bin",
        "junit2otlp",
        f"cat >/dev/null; echo 'traces export: {UNREACHABLE}: connection refused' >&2; exit 0",
    )
    result = _push(scratch, _env(scratch, OTEL_EXPORTER_OTLP_ENDPOINT=UNREACHABLE))
    assert result.returncode == 0, result
    assert "telemetry: push did not land: traces export:" in result.stderr, result
    assert "telemetry: pushed" not in result.stdout, result


def test_push_without_endpoint_is_silent(scratch: Path) -> None:
    _fake(scratch / "bin", "junit2otlp", "echo called >&2; exit 1")
    result = _push(scratch, _env(scratch))
    assert result.returncode == 0, result
    assert result.stdout == "" and result.stderr == "", result


def test_bazel_unreadable_bep_exits_zero_with_warning(scratch: Path) -> None:
    # A stream bep_to_otlp.py cannot read: the script exits 1 before any
    # network call, and the wrapper swallows that with its one line.
    bep = scratch / "bep.json"
    bep.write_text("this is not a build event stream\n", encoding="utf-8")
    env = _env(scratch, OTEL_EXPORTER_OTLP_ENDPOINT=UNREACHABLE)
    task = shutil.which("task", path=env["PATH"])
    assert task, "go-task must be on PATH (run via `task telemetry:self-test`)"
    result = subprocess.run(
        [task, "--taskfile", str(TASKFILE), "bazel", f"BEP={bep}"],
        env=env,
        capture_output=True,
        encoding="utf-8",
        timeout=120,
        check=False,
    )
    assert result.returncode == 0, result
    assert "telemetry: bep_to_otlp.py failed" in result.stderr, result


def _action_body() -> str:
    """The composite step's `run: |` block, de-indented. No YAML library in `test/`."""
    lines = ACTION.read_text(encoding="utf-8").splitlines()
    start = next(i for i, line in enumerate(lines) if line.strip() == "run: |") + 1
    indent = len(lines[start]) - len(lines[start].lstrip())
    body = []
    for line in lines[start:]:
        if line.strip() and len(line) - len(line.lstrip()) < indent:
            break
        body.append(line[indent:])
    return "\n".join(body) + "\n"


def _action(scratch: Path, **extra: str) -> subprocess.CompletedProcess[str]:
    # The step's own `env:` block, except the endpoint, which is pointed at a
    # port nothing listens on.
    version = re.search(r"JUNIT2OTLP_VERSION: (\S+)", ACTION.read_text(encoding="utf-8"))
    assert version, "the action must pin JUNIT2OTLP_VERSION in its step env"
    env = _env(
        scratch,
        JUNIT2OTLP_VERSION=version.group(1),
        RUNNER_OS="Linux",
        RUNNER_ARCH="X64",
        JUNIT=str(scratch / "junit.xml"),
        SUITE="unit",
        OTEL_EXPORTER_OTLP_ENDPOINT=UNREACHABLE,
        GITHUB_SHA="0" * 40,
        GITHUB_REF_NAME="main",
        GITHUB_RUN_ID="1",
        GITHUB_JOB="smoke",
        **extra,
    )
    return subprocess.run(
        ["bash", "-c", _action_body()],
        env=env,
        capture_output=True,
        encoding="utf-8",
        timeout=120,
        check=False,
    )


def test_action_download_failure_exits_zero_with_warning(scratch: Path) -> None:
    # A failing `curl` stands in for an unreachable network: no test here
    # reaches GitHub or otel.ocx.sh.
    _fake(scratch / "bin", "curl", "exit 7")
    result = _action(scratch, OTEL_OTLP_AUTH="Basic Zm9vOmJhcg==")
    assert result.returncode == 0, result
    assert "::warning title=Test telemetry::could not download" in result.stdout, result
    assert "Zm9vOmJhcg==" not in result.stdout + result.stderr, "the secret was echoed"


def test_action_without_secret_is_a_named_no_op(scratch: Path) -> None:
    _fake(scratch / "bin", "curl", "echo curl-called; exit 7")
    result = _action(scratch)
    assert result.returncode == 0, result
    assert result.stdout == "test telemetry: OTEL_OTLP_AUTH is empty, nothing pushed\n", result
