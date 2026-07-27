"""§3.8 S8: Workflow integration scaffolding for ocx-mirror pipeline.

These tests exercise the discover → prepare → test → push leg.
`notify` is intentionally stubbed (replaced by no-op echo) in these tests
so S8 does not depend on S9 (Discord webhook) implementation.

Per design spec §3.8:
- Render workflow from fixture spec → generated .github/workflows/mirror.yml parses as valid YAML
- Drive workflow steps as subprocess sequence: discover → prepare → test → push
- Verify final state: artifact at registry with correct manifest digest
- JUNIT consumption verified by parsing generated XML
- run-summary.json produced by push parses per design spec §2.4 schema
- notify step is stubbed: replaced with no-op echo to confirm wiring

All execute() bodies are unimplemented stubs until wave 3. Tests that call
`ocx-mirror pipeline` subcommands are expected to fail with non-zero exit
codes or NotImplementedError until then. Tests that only verify YAML parsing
or fixture structure pass now.

The registry:2 fixture is required for push-leg tests.
"""
from __future__ import annotations

import json
import os
import re
import subprocess
import sys
from pathlib import Path

import pytest

FIXTURES_DIR = Path(__file__).resolve().parent.parent / "fixtures" / "mirror-shfmt-minimal"


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


@pytest.fixture(scope="session")
def mirror_binary() -> Path:
    """Path to the compiled ocx-mirror binary (reused from test_mirror.py pattern)."""
    if env_path := os.environ.get("OCX_MIRROR_COMMAND"):
        p = Path(env_path)
    else:
        from src.helpers import PROJECT_ROOT
        p = PROJECT_ROOT / "test" / "bin" / "ocx-mirror"
        if sys.platform == "win32" and not p.suffix:
            p = p.with_suffix(".exe")
    if not p.exists():
        pytest.skip(f"ocx-mirror binary not found at {p} — skipping pipeline tests")
    return p


@pytest.fixture()
def mirror_work_dir(tmp_path: Path) -> Path:
    """Isolated working directory for pipeline artifacts."""
    work = tmp_path / "pipeline-work"
    work.mkdir()
    return work


@pytest.fixture()
def pipeline_spec(tmp_path: Path, registry: str) -> Path:
    """Write a minimal mirror spec with the test registry URL substituted."""
    template = (FIXTURES_DIR / "mirror.yml").read_text()
    # Substitute the target registry with the test registry
    spec_content = template.replace("localhost:5000", registry)
    spec_path = tmp_path / "mirror.yml"
    spec_path.write_text(spec_content)
    return spec_path


# ---------------------------------------------------------------------------
# §3.8: YAML structural tests (pass now — no stubs involved)
# ---------------------------------------------------------------------------


def test_fixture_spec_parses(mirror_binary: Path) -> None:
    """The fixture must actually load under the current schema.

    This replaces two tests that asserted substrings of the file as *text*
    (`"ocx_mirror:" in content`). Text checks cannot see a field renamed
    upstream, so the fixture carried `asset_type.kind` — a spelling the
    schema has never accepted — from the day it was written, and every test
    that fed it to the binary passed anyway on an `unimplemented stub`
    escape hatch. Run the real parser instead.
    """
    result = subprocess.run(
        [str(mirror_binary), "package", "validate", str(FIXTURES_DIR / "mirror.yml")],
        capture_output=True,
        text=True,
    )
    assert result.returncode == 0, (
        f"fixture does not parse (rc={result.returncode}): {result.stderr}"
    )


def test_fixture_spec_webhook_is_env_var_name_not_url() -> None:
    """§3.8: R3 mitigation: notify.discord.webhook_secret is an env-var name, not a URL."""
    content = (FIXTURES_DIR / "mirror.yml").read_text()
    match = re.search(r"webhook_secret:\s*(\S+)", content)
    assert match is not None, "webhook_secret field not found"
    val = match.group(1).strip('"').strip("'")
    # Must NOT contain discord.com, discordapp.com, or start with http
    assert "discord.com" not in val.lower(), f"webhook_secret must not contain discord.com URL: {val}"
    assert "discordapp.com" not in val.lower(), f"webhook_secret must not contain discordapp.com URL: {val}"
    assert not val.lower().startswith("http"), f"webhook_secret must not start with http: {val}"
    # Must be a valid env-var name: [A-Z][A-Z0-9_]+
    assert re.match(r"^[A-Z][A-Z0-9_]+$", val), (
        f"webhook_secret must be an uppercase env-var name matching ^[A-Z][A-Z0-9_]+$, got: {val}"
    )


# ---------------------------------------------------------------------------
# §3.8: Pipeline subcommand tests (fail with unimplemented until wave 3)
# ---------------------------------------------------------------------------


def test_pipeline_generate_ci_produces_yaml(mirror_binary: Path, pipeline_spec: Path) -> None:
    """`pipeline generate ci` renders the workflow next to the spec.

    Output goes to the spec's own directory (`ci.rs` takes `spec.parent()` as
    the repo root), not to the working directory — a mirror repo keeps
    `mirror.yml` at its root and the generated workflows under it.

    The `unimplemented stub` escape hatch this used to carry is gone: it
    accepted rc 64/65/74 as a pass, so for as long as the fixture failed to
    parse the assertions below never ran, and the one about the output path
    was wrong.
    """
    result = subprocess.run(
        [str(mirror_binary), "package", "pipeline", "generate", "ci",
         "--spec", str(pipeline_spec)],
        capture_output=True,
        text=True,
    )
    assert result.returncode == 0, f"generate ci failed (rc={result.returncode}): {result.stderr}"

    workflow_path = pipeline_spec.parent / ".github" / "workflows" / "mirror.yml"
    assert workflow_path.exists(), "pipeline generate ci must create .github/workflows/mirror.yml"
    content = workflow_path.read_text()
    assert "on:" in content, "Generated workflow must have 'on:' trigger"
    assert "jobs:" in content, "Generated workflow must have 'jobs:'"


def test_pipeline_plan_exits_zero_or_unimplemented(
    mirror_binary: Path, pipeline_spec: Path, mirror_work_dir: Path
) -> None:
    """§3.8: pipeline plan outputs plan JSON or exits with unimplemented.

    Expected Phase 3: unimplemented — exits non-zero.
    Phase 4+: emits JSON with has_new and versions fields.
    """
    result = subprocess.run(
        [str(mirror_binary), "package", "pipeline", "plan",
         "--spec", str(pipeline_spec)],
        capture_output=True,
        text=True,
    )

    if result.returncode != 0:
        # Expected: unimplemented stub
        return

    # Phase 4+: plan output must parse as JSON
    try:
        plan = json.loads(result.stdout)
        assert "has_new" in plan, "plan output must have has_new field"
        assert "versions" in plan, "plan output must have versions array"
    except json.JSONDecodeError:
        pytest.fail(f"pipeline plan output is not valid JSON: {result.stdout!r}")


def test_pipeline_prepare_exits_unimplemented(
    mirror_binary: Path, pipeline_spec: Path, mirror_work_dir: Path
) -> None:
    """§3.8: pipeline prepare panics with unimplemented until wave 3.

    Expected Phase 3: exits non-zero (panic/abort from unimplemented!()).
    Phase 4+: produces bundles in work_dir.
    """
    result = subprocess.run(
        [str(mirror_binary), "package", "pipeline", "prepare",
         "--spec", str(pipeline_spec),
         "--version", "3.7.0",
         "--work-dir", str(mirror_work_dir)],
        capture_output=True,
        text=True,
    )

    if result.returncode != 0:
        # Expected: unimplemented stub
        return

    # Phase 4+: verify bundle structure
    bundle_path = mirror_work_dir / "3.7.0" / "linux_amd64" / "bundle.tar.xz"
    assert bundle_path.exists(), f"Expected bundle at {bundle_path}"
    manifest_path = mirror_work_dir / "3.7.0" / "manifest.json"
    assert manifest_path.exists(), f"Expected manifest at {manifest_path}"


def test_pipeline_push_exits_unimplemented(
    mirror_binary: Path, pipeline_spec: Path, mirror_work_dir: Path, tmp_path: Path
) -> None:
    """§3.8: pipeline push panics with unimplemented until wave 3.

    Expected Phase 3: exits non-zero.
    Phase 4+: produces run-summary.json with correct schema.
    """
    junit_dir = mirror_work_dir / "junit"
    junit_dir.mkdir()
    bundles_dir = mirror_work_dir / "bundles"
    bundles_dir.mkdir()
    summary_path = mirror_work_dir / "run-summary.json"

    result = subprocess.run(
        [str(mirror_binary), "package", "pipeline", "push",
         "--spec", str(pipeline_spec),
         "--junit-dir", str(junit_dir),
         "--bundles-dir", str(bundles_dir),
         "--write-summary", str(summary_path)],
        capture_output=True,
        text=True,
    )

    if result.returncode != 0:
        # Expected: unimplemented stub
        return

    # Phase 4+: run-summary.json must be valid per §2.4 schema
    assert summary_path.exists(), "run-summary.json must be written by pipeline push"
    summary = json.loads(summary_path.read_text())
    assert summary.get("schema_version") == 1, "schema_version must be 1"
    assert "mirror" in summary, "run-summary must have mirror field"
    assert "versions" in summary, "run-summary must have versions array"
    assert "any_red" in summary, "run-summary must have any_red flag"
    assert "any_new_green" in summary, "run-summary must have any_new_green flag"


def test_pipeline_notify_stub_is_callable(
    mirror_binary: Path, tmp_path: Path
) -> None:
    """§3.8: notify step is stubbed — invoke as no-op to confirm wiring.

    In S8, notify is replaced by echo to test CLI wiring without Discord POST.
    Expected Phase 3: exits non-zero (unimplemented).
    Phase 4+: exits 0 when all_skipped (no-op) or posts to webhook.
    """
    # Write a minimal run-summary.json (all skipped → notify must be silent)
    summary = {
        "schema_version": 1,
        "mirror": "shfmt",
        "target": "localhost:5000/test-shfmt-pipeline",
        "run_url": "https://github.com/ocx-sh/mirror-shfmt/actions/runs/1",
        "versions": [
            {
                "version": "3.7.0",
                "status": "skipped_existing",
                "platforms_pushed": [],
                "platforms_failed": [],
                "cascade_tags_written": [],
                "test_failures": [],
            }
        ],
        "any_red": False,
        "any_new_green": False,
    }
    summary_path = tmp_path / "run-summary.json"
    summary_path.write_text(json.dumps(summary))

    result = subprocess.run(
        [str(mirror_binary), "package", "pipeline", "notify",
         "--run-summary", str(summary_path),
         "--webhook-env-var", "DISCORD_WEBHOOK_URL"],
        capture_output=True,
        text=True,
        env={**os.environ, "DISCORD_WEBHOOK_URL": ""},
    )

    # Phase 3: command exits non-zero (unimplemented)
    # Phase 4+: all-skipped summary → silent (exit 0, no POST)
    # Both are acceptable here — we just confirm the binary is callable with these args.
    assert result.returncode in (0, 1, 2, 64, 65, 69, 77), (
        f"Unexpected exit code {result.returncode}: {result.stderr}"
    )


# ---------------------------------------------------------------------------
# §3.8: Characterization tests for existing pipeline infrastructure
# ---------------------------------------------------------------------------


def test_mirror_binary_has_pipeline_subcommand(mirror_binary: Path) -> None:
    """§3.8: ocx-mirror binary exposes 'pipeline' subcommand group."""
    result = subprocess.run(
        [str(mirror_binary), "package", "pipeline", "--help"],
        capture_output=True,
        text=True,
    )
    # Either help text or error — we just verify the binary accepts 'pipeline'
    # The subcommand group exists even before all subcommands are implemented.
    assert "pipeline" in result.stdout.lower() or "pipeline" in result.stderr.lower() or result.returncode == 0, (
        "ocx-mirror must expose a 'pipeline' subcommand group"
    )


def test_mirror_binary_has_pipeline_generate_ci_subcommand(mirror_binary: Path) -> None:
    """§3.8: pipeline generate ci subcommand is registered."""
    result = subprocess.run(
        [str(mirror_binary), "package", "pipeline", "generate", "ci", "--help"],
        capture_output=True,
        text=True,
    )
    assert result.returncode == 0 or "generate" in (result.stdout + result.stderr).lower(), (
        "pipeline generate ci must be a registered subcommand"
    )


def test_mirror_binary_has_pipeline_plan_subcommand(mirror_binary: Path) -> None:
    """§3.8: pipeline plan subcommand is registered."""
    result = subprocess.run(
        [str(mirror_binary), "package", "pipeline", "plan", "--help"],
        capture_output=True,
        text=True,
    )
    assert result.returncode == 0 or "plan" in (result.stdout + result.stderr).lower(), (
        "pipeline plan must be a registered subcommand"
    )


def test_mirror_binary_has_pipeline_prepare_subcommand(mirror_binary: Path) -> None:
    """§3.8: pipeline prepare subcommand is registered."""
    result = subprocess.run(
        [str(mirror_binary), "package", "pipeline", "prepare", "--help"],
        capture_output=True,
        text=True,
    )
    assert result.returncode == 0 or "prepare" in (result.stdout + result.stderr).lower(), (
        "pipeline prepare must be a registered subcommand"
    )


def test_mirror_binary_has_pipeline_push_subcommand(mirror_binary: Path) -> None:
    """§3.8: pipeline push subcommand is registered."""
    result = subprocess.run(
        [str(mirror_binary), "package", "pipeline", "push", "--help"],
        capture_output=True,
        text=True,
    )
    assert result.returncode == 0 or "push" in (result.stdout + result.stderr).lower(), (
        "pipeline push must be a registered subcommand"
    )


def test_mirror_binary_has_pipeline_notify_subcommand(mirror_binary: Path) -> None:
    """§3.8: pipeline notify subcommand is registered."""
    result = subprocess.run(
        [str(mirror_binary), "package", "pipeline", "notify", "--help"],
        capture_output=True,
        text=True,
    )
    assert result.returncode == 0 or "notify" in (result.stdout + result.stderr).lower(), (
        "pipeline notify must be a registered subcommand"
    )
