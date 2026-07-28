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
import shutil
import subprocess
from pathlib import Path

import pytest

from conftest import WebhookCapture
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
    # ponytail: text grep, not a parse — the locked test env has no yaml module.
    # A malformed workflow carrying both strings still passes. Parse the file
    # (and assert on `jobs` keys) if PyYAML ever lands in the suite for another
    # reason; not worth a new dependency on its own.
    assert "on:" in content, "Generated workflow must have 'on:' trigger"
    assert "jobs:" in content, "Generated workflow must have 'jobs:'"


# ---------------------------------------------------------------------------
# §3.8: Multi-spec repositories
#
# Unit tests cover the renderer and the naming rules. What only a filesystem can
# witness is that a real spec pair on disk renders workflows the drift guard
# then accepts — the generate → check round trip every mirror repo's CI runs.
# ---------------------------------------------------------------------------


@pytest.fixture()
def two_spec_repo(tmp_path: Path) -> Path:
    """A repository holding two mirror specs, each in its own subdirectory.

    The asset port is a literal rather than a live server's: `generate ci` never
    fetches, but the URL still has to parse or `load_spec` rejects the spec.
    """
    repo = tmp_path / "repo"
    for directory in ("shfmt", "shellcheck"):
        spec_dir = repo / directory
        shutil.copytree(FIXTURES_DIR, spec_dir)
        spec = spec_dir / "mirror.yml"
        spec.write_text(
            spec.read_text()
            .replace("__ASSET_PORT__", "9999")
            .replace("test-shfmt-pipeline", f"test-{directory}-pipeline")
            .replace("name: shfmt", f"name: {directory}")
        )
    return repo


def _generate_ci(mirror_binary: Path, *args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [str(mirror_binary), "package", "pipeline", "generate", "ci", *args],
        capture_output=True,
        text=True,
    )


def test_generate_ci_renders_one_workflow_set_per_spec(
    mirror_binary: Path, two_spec_repo: Path
) -> None:
    """§3.8: two specs in subdirectories render two suffixed workflow sets,
    and `--check` accepts them immediately afterwards.

    The round trip is the claim: a renderer whose output its own drift guard
    rejects would red every mirror repo's CI on the commit that generated it.
    """
    spec_args = [
        "--spec", str(two_spec_repo / "shfmt" / "mirror.yml"),
        "--spec", str(two_spec_repo / "shellcheck" / "mirror.yml"),
    ]

    result = _generate_ci(mirror_binary, *spec_args)
    assert result.returncode == 0, f"generate ci failed (rc={result.returncode}): {result.stderr}"

    workflows = two_spec_repo / ".github" / "workflows"
    for name in ("mirror-shfmt.yml", "describe-shfmt.yml", "patch-shfmt.yml",
                 "mirror-shellcheck.yml", "describe-shellcheck.yml", "patch-shellcheck.yml",
                 "verify-generated.yml"):
        assert (workflows / name).exists(), f"{name} was not rendered: {sorted(workflows.iterdir())}"
    # Unsuffixed names belong to a repo-root spec, which this repository has
    # none of; their presence would mean one spec overwrote the other.
    assert not (workflows / "mirror.yml").exists()
    assert not (workflows / "describe.yml").exists()
    assert not (workflows / "patch.yml").exists()

    # Each spec's workflows drive their own spec — a set pointing at the other's
    # would mirror the wrong tool under the right workflow name.
    assert "--spec shfmt/mirror.yml" in (workflows / "mirror-shfmt.yml").read_text()
    assert "--spec shellcheck/mirror.yml" in (workflows / "mirror-shellcheck.yml").read_text()

    check = _generate_ci(mirror_binary, *spec_args, "--check")
    assert check.returncode == 0, f"--check must be green on freshly generated files: {check.stderr}"


def test_generate_ci_check_reds_on_a_hand_edited_workflow(
    mirror_binary: Path, two_spec_repo: Path
) -> None:
    """§3.8 / R4: the drift guard covers every spec's workflows, not just the
    first one's.

    The red half of the round trip above: without it, a `--check` that passed
    unconditionally would satisfy that test.
    """
    spec_args = [
        "--spec", str(two_spec_repo / "shfmt" / "mirror.yml"),
        "--spec", str(two_spec_repo / "shellcheck" / "mirror.yml"),
    ]
    assert _generate_ci(mirror_binary, *spec_args).returncode == 0

    edited = two_spec_repo / ".github" / "workflows" / "mirror-shellcheck.yml"
    edited.write_text(edited.read_text() + "\n# hand-edited\n")

    check = _generate_ci(mirror_binary, *spec_args, "--check")

    assert check.returncode == 65, f"drift must exit 65, got {check.returncode}: {check.stderr}"
    assert "drift: .github/workflows/mirror-shellcheck.yml" in check.stderr, check.stderr


def test_generate_ci_check_reds_on_a_workflow_left_by_a_dropped_spec(
    mirror_binary: Path, two_spec_repo: Path
) -> None:
    """§3.8 / R4: deleting a spec must not leave its workflows running.

    A dropped spec's `mirror-<dir>.yml` keeps its schedule and keeps mirroring
    on a spec nobody maintains, so the guard reports it as stale rather than
    ignoring a file it no longer renders.
    """
    assert _generate_ci(
        mirror_binary,
        "--spec", str(two_spec_repo / "shfmt" / "mirror.yml"),
        "--spec", str(two_spec_repo / "shellcheck" / "mirror.yml"),
    ).returncode == 0

    # The repo root is stated explicitly: with one spec it would otherwise
    # resolve to that spec's own directory, and the check would be looking at an
    # empty `.github/` rather than at the repository's.
    check = _generate_ci(
        mirror_binary,
        "--repo-root", str(two_spec_repo),
        "--spec", str(two_spec_repo / "shfmt" / "mirror.yml"),
        "--check",
    )

    assert check.returncode == 65, f"a stale workflow must exit 65, got {check.returncode}: {check.stderr}"
    assert "stale: .github/workflows/mirror-shellcheck.yml" in check.stderr, check.stderr


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
    assert [v["source_version"] for v in plan["versions"]] == ["3.7.0"]
    # Published version carries the default `build_timestamp`.
    assert re.fullmatch(r"3\.7\.0_\d{14}", plan["versions"][0]["version"])
    assert plan["versions"][0]["platforms"] == ["linux/amd64"]
    assert plan["versions"][0]["assets"][0]["asset_name"] == "shfmt_v3.7.0_linux_amd64"


def test_pipeline_prepare_bundles_the_declared_platform(
    mirror: MirrorRunner, pipeline_spec: Path, mirror_work_dir: Path
) -> None:
    """§3.8: `pipeline prepare` downloads, bundles, and manifests one version.

    Invoked the way the generated `prepare` job invokes it: the version comes
    from the plan, and `--plan` hands over the assets discover already
    resolved. That also pins the timestamped version — `prepare` names its
    manifest after the `--version` argument verbatim but derives the bundle
    directory from the clock, so passing the bare `3.7.0` puts the two in
    different directories.

    The bundle path asserted here is the one `pipeline generate ci` flattens in
    the `prepare` job, so a rename on either side breaks this test.
    """
    plan_path = mirror_work_dir / "plan.json"
    plan_path.write_text(
        mirror.run(
            "package", "pipeline", "plan", "--spec", str(pipeline_spec), "--format", "json"
        ).stdout
    )
    version = json.loads(plan_path.read_text())["versions"][0]["version"]

    result = mirror.run(
        "package", "pipeline", "prepare",
        "--spec", str(pipeline_spec),
        "--version", version,
        "--work-dir", str(mirror_work_dir),
        "--plan", str(plan_path),
    )

    manifest_path = Path(result.stdout.strip())
    assert manifest_path == mirror_work_dir / version / "manifest.json", result.stdout
    version_dir = manifest_path.parent
    bundle_path = version_dir / "linux_amd64" / "bundle.tar.xz"
    assert bundle_path.exists(), f"expected bundle at {bundle_path}"
    # `ocx package push` discovers the metadata sidecar next to the bundle.
    assert (version_dir / "linux_amd64" / "metadata.json").exists()

    manifest = json.loads(manifest_path.read_text())
    assert manifest["version"] == version_dir.name
    assert re.fullmatch(r"3\.7\.0_\d{14}", manifest["version"]), (
        f"default build_timestamp must stamp the version: {manifest['version']}"
    )
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

    # All seven §2.4 required top-level fields.
    summary = json.loads(summary_path.read_text())
    assert summary["schema_version"] == 1
    assert summary["mirror"] == "shfmt"
    assert summary["target"] == f"{mirror.registry}/{unique_mirror_repo}"
    assert summary["run_url"].startswith("https://github.com/"), summary["run_url"]
    assert summary["versions"] == [], "no bundles means no version rows"
    assert summary["any_red"] is False
    assert summary["any_new_green"] is False


def test_pipeline_notify_is_silent_for_an_all_skipped_run(
    mirror: MirrorRunner, webhook_server: WebhookCapture, tmp_path: Path
) -> None:
    """§3.8 / D10: all `skipped_existing` and no failures → exit 0, no POST.

    The webhook is configured and listening, so the empty payload list is an
    assertion rather than an inference: leaving `OCX_MIRROR_DISCORD_HOOK`
    unset would also pass, but only because notify never reaches the lookup —
    silence and never-tried would be indistinguishable.

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

    mirror.env["OCX_MIRROR_DISCORD_HOOK"] = webhook_server.url
    mirror.run("package", "pipeline", "notify", "--run-summary", str(summary_path))

    assert webhook_server.payloads == [], (
        f"all-skipped run must not POST: {webhook_server.payloads}"
    )


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
        ["announce"],
        ["patch"],
    ],
    ids=["group", "generate-ci", "plan", "prepare", "push", "notify", "announce", "patch"],
)
def test_pipeline_subcommand_is_registered(mirror_binary: Path, subcommand: list[str]) -> None:
    """§3.8: every pipeline subcommand answers `--help`.

    Near-identical per-subcommand tests collapsed into one. Each used to assert
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
