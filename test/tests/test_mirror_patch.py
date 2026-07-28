"""#9: `pipeline patch --metadata-only` against the local registry.

Unit tests cover which versions a run selects and the argv it builds. What only
a registry can witness is the claim the whole feature rests on: correcting
published metadata is a manifest re-emission against the layers already in the
registry — no upstream fetch, no layer re-upload — and it strands nobody who
pinned the digest it replaces.

Every test here publishes into its own repository (`unique_mirror_repo`), so
none of them can observe another's tags.

The registry is the witness throughout: the mirror's own log line saying it
republished something is the mirror's account of what it did, and the manifest
the registry hands back is what actually happened.
"""
from __future__ import annotations

import json
import re
import shutil
import urllib.error
import urllib.request
from dataclasses import dataclass
from pathlib import Path

import pytest

from src.mirror_runner import MirrorRunner

#: Media types a manifest read must accept, or registry:2 answers a v2 schema-1
#: manifest whose digest is not the one the index entry names.
MANIFEST_ACCEPT = ", ".join((
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.oci.image.manifest.v1+json",
))

#: The floating tags `cascade: true` advances off the fixture's single version.
CASCADE_ALIASES = ("3.7.0", "3.7", "3", "latest")

#: The one platform the fixture spec declares.
PLATFORM = "linux/amd64"
PLATFORM_SLUG = "linux_amd64"

#: `platforms."linux/amd64".containers[0].image` from the fixture spec, slugged
#: the way `push` slugs it when it looks the JUnit file up.
CONTAINER_ID = "ubuntu_24_04"

#: The env var added to the spec's `metadata.json` to make it drift from what
#: is published. `constant` and `path` are the two `Var` kinds; the fixture
#: already declares a `path`, so this exercises the other one.
EXTRA_ENV_VAR = {"key": "SHFMT_ROOT", "type": "constant", "value": "${installPath}"}


# ---------------------------------------------------------------------------
# Registry reads
# ---------------------------------------------------------------------------


def _tags(registry: str, repository: str) -> frozenset[str]:
    with urllib.request.urlopen(f"http://{registry}/v2/{repository}/tags/list") as resp:
        return frozenset(json.load(resp)["tags"] or [])


def _manifest(registry: str, repository: str, reference: str) -> tuple[dict, str]:
    """The manifest `reference` resolves to, and the digest it resolved to."""
    request = urllib.request.Request(f"http://{registry}/v2/{repository}/manifests/{reference}")
    request.add_header("Accept", MANIFEST_ACCEPT)
    with urllib.request.urlopen(request) as resp:
        return json.load(resp), resp.headers["Docker-Content-Digest"]


def _blob(registry: str, repository: str, digest: str) -> dict:
    with urllib.request.urlopen(f"http://{registry}/v2/{repository}/blobs/{digest}") as resp:
        return json.load(resp)


def _resolves(registry: str, repository: str, reference: str) -> bool:
    try:
        _manifest(registry, repository, reference)
    except urllib.error.HTTPError:
        return False
    return True


@dataclass(frozen=True)
class Published:
    """What the registry holds for one published tag, at one point in time."""

    tags: frozenset[str]
    #: Digest of the image index the tag points at.
    index_digest: str
    #: Digest of the single platform manifest that index carries.
    manifest_digest: str
    #: Layer digests in manifest order — the bytes a patch must not re-upload.
    layers: tuple[str, ...]
    config_digest: str
    #: The package metadata, read out of the config blob where it lives.
    metadata: dict
    #: Cascade alias → the index digest it currently resolves to.
    aliases: dict[str, str]

    @property
    def env_keys(self) -> list[str]:
        return [var["key"] for var in self.metadata["env"]]


def _snapshot(registry: str, repository: str, tag: str) -> Published:
    index, index_digest = _manifest(registry, repository, tag)
    entries = index["manifests"]
    assert len(entries) == 1, f"the fixture declares one platform, got {entries}"
    manifest, manifest_digest = _manifest(registry, repository, entries[0]["digest"])
    return Published(
        tags=_tags(registry, repository),
        index_digest=index_digest,
        manifest_digest=manifest_digest,
        layers=tuple(layer["digest"] for layer in manifest["layers"]),
        config_digest=manifest["config"]["digest"],
        metadata=_blob(registry, repository, manifest["config"]["digest"]),
        aliases={
            alias: _manifest(registry, repository, alias)[1] for alias in CASCADE_ALIASES
        },
    )


# ---------------------------------------------------------------------------
# Publishing the version a patch then corrects
# ---------------------------------------------------------------------------


def _passing_junit(version: str) -> str:
    """JUnit XML for one green (version, platform, container) triple.

    Stands in for the GHA `test` matrix leg the way `test_mirror_e2e.py` does:
    `push` reads nothing from that leg but the XML.
    """
    suite = f"ocx-mirror.shfmt.{PLATFORM_SLUG}.{CONTAINER_ID}"
    return f"""<?xml version="1.0" encoding="UTF-8"?>
<testsuites>
  <testsuite name="{suite}" tests="1" failures="0" errors="0" skipped="0"
             timestamp="2026-05-13T10:00:00Z" time="1.0">
    <properties>
      <property name="ocx.version" value="{version}"/>
      <property name="ocx.platform" value="{PLATFORM}"/>
      <property name="ocx.image" value="ubuntu:24.04"/>
    </properties>
    <testcase name="version" classname="{suite}" time="1.0"/>
  </testsuite>
</testsuites>"""


def _publish(mirror: MirrorRunner, spec: Path, work_dir: Path) -> str:
    """Run plan → prepare → push, and return the published version tag."""
    work_dir.mkdir(parents=True)
    junit_dir = work_dir / "junit"
    junit_dir.mkdir()
    bundles_dir = work_dir / "bundles"
    bundles_dir.mkdir()

    plan_path = work_dir / "plan.json"
    plan_path.write_text(
        mirror.run("package", "pipeline", "plan", "--spec", str(spec), "--format", "json").stdout
    )
    version = json.loads(plan_path.read_text())["versions"][0]["version"]

    mirror.run(
        "package", "pipeline", "prepare",
        "--spec", str(spec),
        "--version", version,
        "--work-dir", str(work_dir),
        "--plan", str(plan_path),
    )

    # The artifact layout `push` consumes, flattened exactly as the generated
    # workflow's `prepare` job does — including its `+` → `_` version slug.
    prepared = work_dir / version.replace("+", "_") / PLATFORM_SLUG
    shutil.copy(prepared / "bundle.tar.xz", bundles_dir / f"bundle-{version}-{PLATFORM_SLUG}.tar.xz")
    shutil.copy(
        prepared / "metadata.json",
        bundles_dir / f"bundle-{version}-{PLATFORM_SLUG}-metadata.json",
    )
    (junit_dir / f"junit-{version}-{PLATFORM_SLUG}-{CONTAINER_ID}.xml").write_text(
        _passing_junit(version)
    )

    mirror.run(
        "package", "pipeline", "push",
        "--spec", str(spec),
        "--junit-dir", str(junit_dir),
        "--bundles-dir", str(bundles_dir),
        "--write-summary", str(work_dir / "run-summary.json"),
    )
    return version


def _declare_extra_env(spec: Path) -> None:
    """Add one env var to the spec's metadata, the way a corrected mirror repo
    would — this is the drift every test below reacts to."""
    metadata_path = spec.parent / "metadata.json"
    metadata = json.loads(metadata_path.read_text())
    metadata["env"].append(EXTRA_ENV_VAR)
    metadata_path.write_text(json.dumps(metadata, indent=2))


def _plan(mirror: MirrorRunner, spec: Path) -> dict:
    return json.loads(
        mirror.run("package", "pipeline", "plan", "--spec", str(spec), "--format", "json").stdout
    )


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


@pytest.fixture()
def published(mirror: MirrorRunner, pipeline_spec: Path, tmp_path: Path, asset_server) -> str:
    """One version of the fixture mirror, published through the real pipeline."""
    version = _publish(mirror, pipeline_spec, tmp_path / "publish")
    assert re.fullmatch(r"3\.7\.0_\d{14}", version), version
    # The recorder has to be provably live before its silence during the patch
    # can mean anything: an empty list is otherwise equally consistent with a
    # patch that fetched nothing and a fixture that records nothing.
    assert asset_server.requests, "the publish must have fetched the upstream asset"
    return version


@dataclass(frozen=True)
class Patched:
    """A published version, its metadata corrected in the spec, and patched."""

    version: str
    before: Published
    after: Published
    #: Upstream asset requests the registry saw *during the patch*.
    fetches: list[str]


@pytest.fixture()
def patched(
    mirror: MirrorRunner,
    pipeline_spec: Path,
    published: str,
    registry: str,
    unique_mirror_repo: str,
    asset_server,
) -> Patched:
    before = _snapshot(registry, unique_mirror_repo, published)
    _declare_extra_env(pipeline_spec)

    fetches_before = len(asset_server.requests)
    mirror.run("package", "pipeline", "patch", "--metadata-only", "--spec", str(pipeline_spec))

    return Patched(
        version=published,
        before=before,
        after=_snapshot(registry, unique_mirror_repo, published),
        fetches=asset_server.requests[fetches_before:],
    )


# ---------------------------------------------------------------------------
# plan — the drift signal
# ---------------------------------------------------------------------------


def test_plan_reports_no_drift_for_a_freshly_published_version(
    mirror: MirrorRunner, pipeline_spec: Path, published: str
) -> None:
    """#9: a mirror whose spec nobody touched has nothing to patch.

    The other half of `test_plan_reports_metadata_drift_...`: without this, a
    `has_drift` stuck at `true` would satisfy that test and nothing would red.
    """
    plan = _plan(mirror, pipeline_spec)

    assert plan["has_drift"] is False, plan
    assert plan["has_new"] is False, plan
    assert plan["versions"] == [], plan


def test_plan_reports_metadata_drift_after_the_spec_metadata_changes(
    mirror: MirrorRunner, pipeline_spec: Path, published: str
) -> None:
    """#9: correcting `metadata.json` makes the published version drift."""
    _declare_extra_env(pipeline_spec)

    plan = _plan(mirror, pipeline_spec)

    assert plan["has_drift"] is True, plan
    # Drift schedules no download, so the flag the generated workflow gates its
    # build jobs on must stay down.
    assert plan["has_new"] is False, plan
    assert [v["version"] for v in plan["versions"]] == [published], plan
    entry = plan["versions"][0]
    assert entry["kind"] == "metadata-drift", entry
    assert entry["platforms"] == [PLATFORM], entry
    # A patch re-references published layers; there is no upstream asset to
    # resolve and no upstream version string to recover from a normalized tag.
    assert entry["assets"] == [], entry
    assert entry["source_version"] == "", entry


# ---------------------------------------------------------------------------
# patch — what it rewrites, and what it must leave alone
# ---------------------------------------------------------------------------


def test_patch_publishes_the_metadata_the_spec_now_declares(patched: Patched) -> None:
    """#9: the published config blob carries the corrected metadata."""
    assert patched.before.env_keys == ["PATH"], patched.before.metadata
    assert patched.after.env_keys == ["PATH", EXTRA_ENV_VAR["key"]], patched.after.metadata

    published_var = patched.after.metadata["env"][-1]
    assert published_var["type"] == EXTRA_ENV_VAR["type"], published_var
    assert published_var["value"] == EXTRA_ENV_VAR["value"], published_var
    assert patched.after.config_digest != patched.before.config_digest


def test_patch_re_references_every_published_layer(patched: Patched) -> None:
    """#9: the patched manifest names the layers already in the registry.

    Half of the cheap-patch claim. It proves the re-emitted manifest points at
    the same blobs — *not* on its own that nothing was re-uploaded: the fixture
    bundle is byte-reproducible, so a full re-mirror would land on this same
    digest. `test_patch_fetches_nothing_from_upstream` is what rules that out;
    the two together are the claim.
    """
    assert patched.after.layers == patched.before.layers, (
        f"a metadata patch must not touch the layer stack: "
        f"{patched.before.layers} → {patched.after.layers}"
    )
    assert patched.after.layers, "the fixture must publish at least one layer"


def test_patch_fetches_nothing_from_upstream(patched: Patched) -> None:
    """#9: no upstream asset is downloaded — the point of patching over
    re-mirroring, and the reason it takes seconds instead of hours.

    This is the half of the layer-reuse claim the digests cannot carry: equal
    digests hold just as well for a patch that re-downloaded and re-uploaded
    identical bytes. Silence on the asset server rules that out. The `published`
    fixture asserts the recorder saw the publish, so an empty list here means
    "did not fetch", not "nothing is watching".
    """
    assert patched.fetches == [], f"patch fetched upstream assets: {patched.fetches}"


def test_patch_moves_the_manifest_digest(patched: Patched) -> None:
    """#9: something was actually republished.

    An unchanged digest would mean the run reported success without re-emitting
    anything — and `test_patch_publishes_the_metadata...` alone cannot tell that
    apart from a registry that never held the old metadata.
    """
    assert patched.after.index_digest != patched.before.index_digest
    assert patched.after.manifest_digest != patched.before.manifest_digest


def test_patch_evicts_nothing_a_consumer_could_have_pinned(
    patched: Patched, registry: str, unique_mirror_repo: str
) -> None:
    """#9: a consumer pinned to the pre-patch digest keeps resolving.

    Patching is not allowed to evict what it replaces: `ocx.lock` pins
    `@sha256:`, and a patch that orphaned those pins would break every project
    that had installed the version — a far worse outcome than the stale
    metadata it fixes.
    """
    assert _resolves(registry, unique_mirror_repo, patched.before.index_digest), (
        f"the pre-patch image index {patched.before.index_digest} no longer resolves"
    )
    assert _resolves(registry, unique_mirror_repo, patched.before.manifest_digest), (
        f"the pre-patch platform manifest {patched.before.manifest_digest} no longer resolves"
    )
    # Tags are the other pinning surface — including the digest-named
    # `sha256.<hex>` canonical tags, when the `ocx` driving the push writes them.
    assert patched.before.tags <= patched.after.tags, (
        f"a patch dropped published tags: {sorted(patched.before.tags - patched.after.tags)}"
    )


def test_cascade_aliases_follow_the_patched_manifest(patched: Patched) -> None:
    """#9: `3.7.0`, `3.7`, `3` and `latest` point at the re-emitted index.

    An alias left on the old index is the failure this would otherwise hide:
    the leaf tag would carry corrected metadata while every floating tag a user
    actually installs still served the stale one.
    """
    assert patched.before.aliases == {a: patched.before.index_digest for a in CASCADE_ALIASES}
    assert patched.after.aliases == {a: patched.after.index_digest for a in CASCADE_ALIASES}


# ---------------------------------------------------------------------------
# patch — idempotence and selection errors
# ---------------------------------------------------------------------------


def test_a_second_patch_reports_no_drift_and_republishes_nothing(
    patched: Patched, mirror: MirrorRunner, pipeline_spec: Path, registry: str, unique_mirror_repo: str
) -> None:
    """#9: the comparison against the registry is the whole idempotency
    mechanism — there is no ledger, so a re-run must settle itself."""
    assert _plan(mirror, pipeline_spec)["has_drift"] is False

    mirror.run("package", "pipeline", "patch", "--metadata-only", "--spec", str(pipeline_spec))

    settled = _snapshot(registry, unique_mirror_repo, patched.version)
    assert settled.index_digest == patched.after.index_digest, (
        "a patch with nothing to do must not re-emit the manifest"
    )
    assert settled.manifest_digest == patched.after.manifest_digest


def test_patch_rejects_a_version_the_registry_does_not_publish(
    mirror: MirrorRunner, pipeline_spec: Path, published: str
) -> None:
    """#9: a `--version` nobody published is a typo, not a no-op.

    Exiting 0 there is how a corrected spec quietly stays unpublished while the
    run reports success.
    """
    result = mirror.run(
        "package", "pipeline", "patch", "--metadata-only",
        "--spec", str(pipeline_spec),
        "--version", "9.9.9",
        check=False,
    )

    assert result.returncode == 64, f"expected a usage error, got {result.returncode}: {result.stderr}"
    assert "9.9.9" in result.stderr, result.stderr
