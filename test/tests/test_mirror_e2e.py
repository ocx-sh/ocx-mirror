"""§3.10 S10: End-to-end acceptance test for ocx-mirror pipeline.

Exercises the full pipeline: plan → prepare → (test) → push → notify against
the local registry. Both external dependencies are stood up locally: the
upstream asset comes from the `asset_server` fixture and the Discord webhook
is intercepted by a capture server, so no network egress is required.

Per design spec §3.10:
- Run the full pipeline against registry:2
- Assertions: a version published and its cascade tags present *in the
  registry*, JUnit consumed, Discord POST captured with the D10 colour

The `test` leg is represented by its output — a JUnit file — rather than by
running containers: `push` reads nothing else from it.
"""
from __future__ import annotations

import json
import re
import shutil
import urllib.request
from pathlib import Path

from conftest import WebhookCapture
from src.mirror_runner import MirrorRunner


def _registry_tags(registry: str, repository: str) -> set[str]:
    """Tags the registry actually carries for ``repository``."""
    with urllib.request.urlopen(f"http://{registry}/v2/{repository}/tags/list") as resp:
        return set(json.load(resp)["tags"] or [])


# ---------------------------------------------------------------------------
# §3.10: Harness self-tests for the webhook capture server.
#
# These two assert on a payload the test itself just POSTed — the shape the
# five deleted `test_run_summary_*` tests had. Kept deliberately: the e2e and
# the all-skipped notify test both assert on `payloads`, and an empty list
# only means silence if the server provably records what it receives.
# ---------------------------------------------------------------------------


def test_webhook_server_accepts_post(webhook_server: WebhookCapture) -> None:
    """§3.10: Local webhook tracking server captures POST payloads correctly."""

    payload = json.dumps({"username": "ocx-mirror", "embeds": [{"title": "test"}]}).encode()
    req = urllib.request.Request(
        webhook_server.url,
        data=payload,
        method="POST",
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req) as resp:
        assert resp.status == 204, f"Webhook server must return 204, got {resp.status}"

    assert len(webhook_server.payloads) == 1, "Exactly one payload must be captured"
    assert webhook_server.payloads[0]["username"] == "ocx-mirror"


def test_webhook_server_captures_discord_embed_shape(webhook_server: WebhookCapture) -> None:
    """§3.10: Webhook server correctly captures Discord embed payload structure."""

    payload = json.dumps({
        "username": "ocx-mirror",
        "embeds": [{
            "title": "📦 shfmt: published 3.7.0",
            "color": 3066993,
            "url": "https://github.com/ocx-sh/mirror-shfmt/actions/runs/1",
            "fields": [
                {"name": "Platforms", "value": "linux/amd64", "inline": False},
                {"name": "Cascade", "value": "3.7.0, 3.7, 3, latest", "inline": False},
            ],
        }],
    }).encode()
    req = urllib.request.Request(
        webhook_server.url,
        data=payload,
        method="POST",
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req) as _:
        pass

    assert len(webhook_server.payloads) >= 1
    captured = webhook_server.payloads[-1]
    assert "embeds" in captured, "Discord payload must have embeds array"
    embed = captured["embeds"][0]
    assert "title" in embed, "Embed must have title"
    assert "color" in embed, "Embed must have color"


# ---------------------------------------------------------------------------
# §3.10: Full pipeline e2e
#
# Five `test_run_summary_*` tests used to sit here. Each built a dict inline
# and asserted that the keys it had just written were present, or that
# `0x2ECC71 == 3_066_993`. No product code was involved, so they could not
# fail — the same unfalsifiable-green defect as the exit-code bands of #31.
# The schema and the D10 colour are now asserted against real `push` and
# `notify` output below.
# ---------------------------------------------------------------------------


#: `platforms."linux/amd64".containers[0].image` from the fixture spec, slugged
#: the way `push` slugs it when it looks the JUnit file up.
CONTAINER_IMAGE = "ubuntu:24.04"
CONTAINER_ID = "ubuntu_24_04"


def _passing_junit(version: str, platform: str) -> str:
    """JUnit XML for one green (version, platform, container) triple.

    Stands in for the GHA `test` matrix leg, which runs the spec's container
    tests and uploads exactly this file. Running real containers here would
    make the suite depend on pullable base images; `push` only ever reads the
    XML, so producing it directly exercises the same code path.
    """
    slug = platform.replace("/", "_")
    cid = CONTAINER_ID
    image = CONTAINER_IMAGE
    return f"""<?xml version="1.0" encoding="UTF-8"?>
<testsuites>
  <testsuite name="ocx-mirror.shfmt.{slug}.{cid}"
             tests="1" failures="0" errors="0" skipped="0"
             timestamp="2026-05-13T10:00:00Z" time="1.0">
    <properties>
      <property name="ocx.version" value="{version}"/>
      <property name="ocx.platform" value="{platform}"/>
      <property name="ocx.image" value="{image}"/>
    </properties>
    <testcase name="version" classname="ocx-mirror.shfmt.{slug}.{cid}" time="1.0"/>
  </testsuite>
</testsuites>"""


def test_full_pipeline_against_registry(
    mirror: MirrorRunner,
    registry: str,
    unique_mirror_repo: str,
    pipeline_spec: Path,
    webhook_server: WebhookCapture,
    tmp_path: Path,
) -> None:
    """§3.10: Full pipeline: plan → prepare → (test) → push → notify.

    This test spent months reporting "skipped: pipeline plan unimplemented"
    while the real exit was 69 `SourceError` — the fixture's `__ASSET_PORT__`
    placeholder was never substituted, so the upstream asset URL was
    unparseable and no pipeline leg could have run (issue #31). The
    `pipeline_spec` fixture now resolves all three placeholders and the four
    skips below are gone: every leg is asserted to exit 0.
    """
    work_dir = tmp_path / "pipeline-work"
    work_dir.mkdir()
    junit_dir = work_dir / "junit"
    junit_dir.mkdir()
    bundles_dir = work_dir / "bundles"
    bundles_dir.mkdir()
    summary_path = work_dir / "run-summary.json"

    # Step 1: plan — discover the versions the target does not carry. The
    # workflow's `discover` job persists this as plan.json; so does this test,
    # because step 2 consumes it.
    plan_path = work_dir / "plan.json"
    plan_path.write_text(
        mirror.run(
            "package", "pipeline", "plan", "--spec", str(pipeline_spec), "--format", "json"
        ).stdout
    )
    plan = json.loads(plan_path.read_text())
    assert plan["has_new"] is True
    version = plan["versions"][0]["version"]
    platform = plan["versions"][0]["platforms"][0]
    platform_slug = platform.replace("/", "_")
    # Default `build_timestamp` — the setting real mirrors run on. Asserted so
    # a regression to `none` cannot quietly turn the cascade below into a
    # weaker check.
    assert re.fullmatch(r"3\.7\.0_\d{14}", version), version

    # Step 2: prepare — download, verify, bundle. `--plan` reuses the asset
    # URLs discover already resolved, as the workflow does; it also pins the
    # timestamped version, which prepare would otherwise recompute from the
    # clock and could land a second later than plan did.
    mirror.run(
        "package", "pipeline", "prepare",
        "--spec", str(pipeline_spec),
        "--version", version,
        "--work-dir", str(work_dir),
        "--plan", str(plan_path),
    )

    # Step 3: flatten to the artifact layout `push` consumes, exactly as the
    # generated workflow's `prepare` job does — including its `+` → `_` slug
    # of the version directory.
    prepared = work_dir / version.replace("+", "_") / platform_slug
    shutil.copy(prepared / "bundle.tar.xz", bundles_dir / f"bundle-{version}-{platform_slug}.tar.xz")
    shutil.copy(prepared / "metadata.json", bundles_dir / f"bundle-{version}-{platform_slug}-metadata.json")
    (junit_dir / f"junit-{version}-{platform_slug}-{CONTAINER_ID}.xml").write_text(
        _passing_junit(version, platform)
    )

    # Step 4: push — publish the green (V, P) pairs and write the run summary.
    mirror.run(
        "package", "pipeline", "push",
        "--spec", str(pipeline_spec),
        "--junit-dir", str(junit_dir),
        "--bundles-dir", str(bundles_dir),
        "--write-summary", str(summary_path),
    )

    summary = json.loads(summary_path.read_text())
    assert summary["any_new_green"] is True
    assert summary["any_red"] is False
    published = summary["versions"][0]
    assert published["status"] == "published", published
    assert published["platforms_pushed"] == [platform]
    # The build-tagged version plus the four floating tags it advances.
    assert published["cascade_tags_written"] == [version, "3.7.0", "3.7", "3", "latest"]

    # §3.10: the registry is the only witness that matters — the summary is
    # the mirror's own account of what it did, the tag list is the world's.
    tags = _registry_tags(registry, unique_mirror_repo)
    assert set(published["cascade_tags_written"]) <= tags, (
        f"cascade tags claimed in run-summary.json are missing from the registry: {tags}"
    )

    # Step 5: notify — POST to the local capture server.
    mirror.env["OCX_MIRROR_DISCORD_HOOK"] = webhook_server.url
    mirror.run("package", "pipeline", "notify", "--run-summary", str(summary_path))

    assert len(webhook_server.payloads) == 1, (
        f"one published version means one Discord message: {webhook_server.payloads}"
    )
    embed = webhook_server.payloads[0]["embeds"][0]
    assert embed["title"] == f"{registry}/{unique_mirror_repo}: {version} published"
    # §2.5: GREEN = 0x2ECC71 for a run with no red.
    assert embed["color"] == 0x2ECC71, f"all-green run must be green, got {embed['color']:#x}"
