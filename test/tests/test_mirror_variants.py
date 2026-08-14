"""Acceptance tests for variant-aware push and the default-variant alias pass.

`push_and_cascade` (`src/pipeline/push.rs`) does two things no other push path
does: it cascades a variant-prefixed version along its own track
(`full-1.2.3` → `full-1.2` → `full` — the variant name IS the track's rolling
`latest`), and, for the variant marked `default: true`, it pushes a SECOND
cascade of unadorned tags (`1.2.3` → `1.2` → `1` → `latest`) pointing at the
same manifest.

That second pass is the whole reason a repository with variants still answers
to a bare `1.2.3`. It has no unit coverage — the alias tags only exist after a
real `push_cascade` against a real registry — so it is pinned here, against the
`registry` fixture, by reading the tag list and the manifest digests back
off the registry HTTP API.
"""
from __future__ import annotations

import json
import shutil
import stat
import sys
import tarfile
import urllib.request
from pathlib import Path

from src.mirror_runner import MirrorRunner
from src.runner import current_platform

FIXTURES_DIR = Path(__file__).resolve().parent.parent / "fixtures" / "mirror" / "test-tool"

#: Media types a manifest read must accept, or registry:2 answers a v2 schema-1
#: manifest whose digest is not the one the index entry names.
MANIFEST_ACCEPT = ", ".join((
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.oci.image.manifest.v1+json",
))

VERSION = "1.2.3"

#: What `cascade: true` advances off `VERSION` on a variant's own track. The
#: rolling `latest` of a variant track is the bare variant name — `latest`
#: itself belongs to the default variant's alias pass alone.
def _variant_track(name: str) -> tuple[str, ...]:
    return (f"{name}-{VERSION}", f"{name}-1.2", f"{name}-1", name)


#: The alias pass the default variant adds on top of its own track.
BARE_TRACK = (VERSION, "1.2", "1", "latest")


# ---------------------------------------------------------------------------
# Registry reads
# ---------------------------------------------------------------------------


def _tags(registry: str, repository: str) -> frozenset[str]:
    with urllib.request.urlopen(f"http://{registry}/v2/{repository}/tags/list") as resp:
        return frozenset(json.load(resp)["tags"] or [])


def _digest(registry: str, repository: str, reference: str) -> str:
    """The image-index digest `reference` resolves to."""
    request = urllib.request.Request(f"http://{registry}/v2/{repository}/manifests/{reference}")
    request.add_header("Accept", MANIFEST_ACCEPT)
    with urllib.request.urlopen(request) as resp:
        return resp.headers["Docker-Content-Digest"]


# ---------------------------------------------------------------------------
# Spec construction
# ---------------------------------------------------------------------------


def _make_tarball(tmp_path: Path, name: str, marker: str) -> Path:
    """A .tar.gz holding `bin/test-tool` that echoes `marker`.

    Each variant gets its own marker so the two tracks cannot accidentally
    resolve to one shared manifest and make the digest assertions vacuous.
    """
    bin_dir = tmp_path / f"pkg-{name}" / "bin"
    bin_dir.mkdir(parents=True, exist_ok=True)

    script = bin_dir / "test-tool"
    if sys.platform == "win32":
        script = script.with_suffix(".bat")
        script.write_text(f"@echo {marker}\n")
    else:
        script.write_text(f"#!/bin/sh\necho {marker}\n")
        script.chmod(script.stat().st_mode | stat.S_IEXEC)

    tarball = tmp_path / f"{name}.tar.gz"
    with tarfile.open(tarball, "w:gz") as tar:
        tar.add(bin_dir, arcname="bin")
    return tarball


def _write_variant_spec(
    path: Path,
    *,
    registry: str,
    repo: str,
    asset_server,
    cascade: bool = True,
) -> None:
    """A two-variant spec: `full` (default) and `slim`, one version, one platform.

    Both variants resolve on the host platform from distinct assets, so every
    tag either suite asserts on is one this host actually publishes.
    """
    host = current_platform()
    metadata_path = str(FIXTURES_DIR / "metadata.json")
    path.write_text(
        "\n".join(
            [
                "name: test-tool",
                "source:",
                "  type: url_index",
                "  versions:",
                f'    "{VERSION}":',
                "      assets:",
                f'        "test-tool-full.tar.gz": "{asset_server.url("test-tool-full.tar.gz")}"',
                f'        "test-tool-slim.tar.gz": "{asset_server.url("test-tool-slim.tar.gz")}"',
                "variants:",
                "  - name: full",
                "    default: true",
                "    assets:",
                f'      "{host}":',
                "        - '^test-tool-full\\.tar\\.gz$'",
                "  - name: slim",
                "    assets:",
                f'      "{host}":',
                "        - '^test-tool-slim\\.tar\\.gz$'",
                "target:",
                f'  registry: "{registry}"',
                f'  repository: "{repo}"',
                "metadata:",
                f'  default: "{metadata_path}"',
                f"cascade: {str(cascade).lower()}",
                "build_timestamp: none",
            ]
        )
        + "\n"
    )


def _sync_two_variants(
    mirror: MirrorRunner,
    tmp_path: Path,
    registry: str,
    repo: str,
    asset_server,
    *,
    cascade: bool = True,
) -> None:
    shutil.copy(_make_tarball(tmp_path, "full", "marker-full"), asset_server.dir / "test-tool-full.tar.gz")
    shutil.copy(_make_tarball(tmp_path, "slim", "marker-slim"), asset_server.dir / "test-tool-slim.tar.gz")

    spec_path = tmp_path / "mirror-variants.yaml"
    _write_variant_spec(spec_path, registry=registry, repo=repo, asset_server=asset_server, cascade=cascade)
    mirror.run("package", "sync", str(spec_path), "--work-dir", str(mirror.temp_dir))


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------


def test_sync_publishes_both_variant_tracks(
    mirror: MirrorRunner, tmp_path: Path, registry: str, unique_mirror_repo: str, asset_server,
):
    """Each variant cascades along its own prefixed track, `latest` included.

    A variant's rolling `latest` is the bare variant name, so the two tracks
    never collide on a shared floating tag — which is what lets both be pushed
    into one repository in a single run.
    """
    _sync_two_variants(mirror, tmp_path, registry, unique_mirror_repo, asset_server)

    tags = _tags(registry, unique_mirror_repo)
    for expected in _variant_track("full") + _variant_track("slim"):
        assert expected in tags, f"expected tag '{expected}' in {sorted(tags)}"


def test_default_variant_aliases_the_bare_tags_to_its_own_manifest(
    mirror: MirrorRunner, tmp_path: Path, registry: str, unique_mirror_repo: str, asset_server,
):
    """The default variant's second cascade pass owns every unadorned tag.

    Digest equality against `full-1.2.3` is the assertion that matters: tag
    presence alone would also pass if the alias pass had pushed a fresh,
    unrelated manifest, or if `slim` — the last variant pushed in the run — had
    taken the bare tags by writing them last.
    """
    _sync_two_variants(mirror, tmp_path, registry, unique_mirror_repo, asset_server)

    tags = _tags(registry, unique_mirror_repo)
    for expected in BARE_TRACK:
        assert expected in tags, f"expected alias tag '{expected}' in {sorted(tags)}"

    default_digest = _digest(registry, unique_mirror_repo, f"full-{VERSION}")
    slim_digest = _digest(registry, unique_mirror_repo, f"slim-{VERSION}")
    assert default_digest != slim_digest, "the two variants must publish distinct manifests for this to prove anything"

    for alias in BARE_TRACK:
        assert _digest(registry, unique_mirror_repo, alias) == default_digest, (
            f"bare tag '{alias}' must resolve to the default variant's manifest, not {slim_digest}"
        )


def test_no_bare_alias_without_cascade(
    mirror: MirrorRunner, tmp_path: Path, registry: str, unique_mirror_repo: str, asset_server,
):
    """`cascade: false` publishes the variant tags only — no alias pass.

    The alias pass lives inside the cascade branch, so a variants spec that
    turns cascade off gets no unadorned tag at all: consumers of that
    repository must name a variant. Pinned because the alternative reading —
    "the default variant is always reachable bare" — is the intuitive one, and
    nothing else in the suite would catch the change.
    """
    _sync_two_variants(mirror, tmp_path, registry, unique_mirror_repo, asset_server, cascade=False)

    tags = _tags(registry, unique_mirror_repo)
    assert f"full-{VERSION}" in tags, f"the variant's own tag must still be published; got {sorted(tags)}"
    assert f"slim-{VERSION}" in tags, f"the variant's own tag must still be published; got {sorted(tags)}"
    for absent in BARE_TRACK:
        assert absent not in tags, f"'{absent}' must not exist without cascade; got {sorted(tags)}"
