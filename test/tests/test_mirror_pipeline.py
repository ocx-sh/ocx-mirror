"""§3.8 S8: Workflow integration for the ocx-mirror pipeline.

These tests drive the discover → prepare → push leg one subcommand at a time
against the local registry. The full sequence, including notify, lives in
`test_mirror_e2e.py`.

Every command here is asserted to exit 0. The `if rc != 0: return` hatches
that used to guard `plan`, `prepare` and `push` were Phase-3 scaffolding from
when the bodies were `unimplemented!()`; once implemented they could no longer
tell "still a stub" from "the binary rejected my input", and that is how the
fixture spent seven weeks failing to parse with the suite green (issue #31).

The registry:2 fixture is required — `plan` lists the target's tags.
"""
from __future__ import annotations

import json
import re
import subprocess
from pathlib import Path

import pytest

from src.mirror_runner import MirrorRunner

FIXTURES_DIR = Path(__file__).resolve().parent.parent / "fixtures" / "mirror-shfmt-minimal"


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


@pytest.fixture()
def mirror_work_dir(tmp_path: Path) -> Path:
    """Isolated working directory for pipeline artifacts."""
    work = tmp_path / "pipeline-work"
    work.mkdir()
    return work


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


def test_pipeline_plan_reports_the_unmirrored_version(
    mirror: MirrorRunner, pipeline_spec: Path
) -> None:
    """§3.8: `pipeline plan` names the version the target does not carry yet.

    Asserting on the parsed plan is what makes this a check: the previous
    version of this test read stdout without `--format json`, so the plain
    renderer's output would never have parsed — but the `rc != 0: return`
    hatch above it meant the parse was never reached.
    """
    result = mirror.run(
        "package", "pipeline", "plan", "--spec", str(pipeline_spec), "--format", "json"
    )

    plan = json.loads(result.stdout)
    assert plan["has_new"] is True, f"empty target must have new work: {plan}"
    assert [v["version"] for v in plan["versions"]] == ["3.7.0"]
    assert plan["versions"][0]["platforms"] == ["linux/amd64"]
    assert plan["versions"][0]["assets"][0]["asset_name"] == "shfmt_v3.7.0_linux_amd64"


def test_pipeline_prepare_bundles_the_declared_platform(
    mirror: MirrorRunner, pipeline_spec: Path, mirror_work_dir: Path
) -> None:
    """§3.8: `pipeline prepare` downloads, bundles, and manifests one version.

    The bundle path asserted here is the one `pipeline generate ci` flattens in
    the `prepare` job, so a rename on either side breaks this test.
    """
    mirror.run(
        "package", "pipeline", "prepare",
        "--spec", str(pipeline_spec),
        "--version", "3.7.0",
        "--work-dir", str(mirror_work_dir),
    )

    version_dir = mirror_work_dir / "3.7.0"
    bundle_path = version_dir / "linux_amd64" / "bundle.tar.xz"
    assert bundle_path.exists(), f"expected bundle at {bundle_path}"
    # `ocx package push` discovers the metadata sidecar next to the bundle.
    assert (version_dir / "linux_amd64" / "metadata.json").exists()

    manifest = json.loads((version_dir / "manifest.json").read_text())
    assert manifest["version"] == "3.7.0"
    assert [b["platform_slug"] for b in manifest["bundles"]] == ["linux_amd64"]
    assert manifest["bundles"][0]["bundle_path"] == str(bundle_path)
    assert manifest["bundles"][0]["size_bytes"] == bundle_path.stat().st_size


def test_pipeline_push_with_no_bundles_reports_nothing_published(
    mirror: MirrorRunner, pipeline_spec: Path, mirror_work_dir: Path, unique_mirror_repo: str
) -> None:
    """§3.8: an empty bundles directory is a clean no-op, not a failure.

    A `discover` that found no work leaves `push` with empty artifact
    directories; that run must exit 0 and say so in the summary rather than
    red the workflow. The publishing path is covered by
    `test_mirror_e2e.py::test_full_pipeline_against_registry`.
    """
    junit_dir = mirror_work_dir / "junit"
    junit_dir.mkdir()
    bundles_dir = mirror_work_dir / "bundles"
    bundles_dir.mkdir()
    summary_path = mirror_work_dir / "run-summary.json"

    mirror.run(
        "package", "pipeline", "push",
        "--spec", str(pipeline_spec),
        "--junit-dir", str(junit_dir),
        "--bundles-dir", str(bundles_dir),
        "--write-summary", str(summary_path),
    )

    summary = json.loads(summary_path.read_text())
    assert summary["schema_version"] == 1
    assert summary["mirror"] == "shfmt"
    assert summary["target"] == f"{mirror.registry}/{unique_mirror_repo}"
    assert summary["versions"] == [], "no bundles means no version rows"
    assert summary["any_red"] is False
    assert summary["any_new_green"] is False


def test_pipeline_notify_is_silent_for_an_all_skipped_run(
    mirror: MirrorRunner, tmp_path: Path
) -> None:
    """§3.8 / D10: all `skipped_existing` and no failures → exit 0, no POST.

    `OCX_MIRROR_DISCORD_HOOK` is deliberately left unset: notify must return
    before it ever looks the webhook up. If it started POSTing on an
    all-skipped run this would exit 78 on the missing variable.

    The old version of this test passed `--webhook-env-var`, a flag notify has
    never had, and accepted `rc in (0, 1, 2, 64, 65, 69, 77)` — so clap's
    exit-2 usage error read as a pass.
    """
    summary_path = tmp_path / "run-summary.json"
    summary_path.write_text(json.dumps({
        "schema_version": 1,
        "mirror": "shfmt",
        "target": f"{mirror.registry}/test-shfmt-pipeline",
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
    }))

    mirror.run("package", "pipeline", "notify", "--run-summary", str(summary_path))


# ---------------------------------------------------------------------------
# §3.8: Characterization tests for existing pipeline infrastructure
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "subcommand",
    [
        [],
        ["generate", "ci"],
        ["plan"],
        ["prepare"],
        ["push"],
        ["notify"],
    ],
    ids=["group", "generate-ci", "plan", "prepare", "push", "notify"],
)
def test_pipeline_subcommand_is_registered(mirror_binary: Path, subcommand: list[str]) -> None:
    """§3.8: every pipeline subcommand answers `--help`.

    Six near-identical tests collapsed into one. Each used to assert
    `rc == 0 or "<name>" in output` — but clap prints the subcommand name in
    its usage error too, so the disjunction held for an unregistered command
    as well. `--help` on a registered command exits 0; that is the whole check.
    """
    result = subprocess.run(
        [str(mirror_binary), "package", "pipeline", *subcommand, "--help"],
        capture_output=True,
        text=True,
    )
    assert result.returncode == 0, (
        f"`pipeline {' '.join(subcommand)} --help` exited {result.returncode}: {result.stderr}"
    )
