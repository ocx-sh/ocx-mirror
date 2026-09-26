"""#90: a spec dependency named by tag alone is pinned per platform.

`pipeline prepare` (and `package sync`, the same code path) hands each
platform's content tree to `ocx package create`, which resolves a tag-only
dependency to *that platform's* leaf manifest digest. The registry is the
witness: the digest in each sidecar must be the one the dependency's image
index lists for the same platform, and a published mirror must not read as
drifted against the tag-only spec it was published from.
"""
from __future__ import annotations

import json
import urllib.request
import uuid
from pathlib import Path

from src.helpers import push_stub_ocx_package
from src.mirror_runner import MirrorRunner

PLATFORMS = ("linux/amd64", "linux/arm64")


def _leaf_digests(registry: str, repository: str, tag: str) -> dict[str, str]:
    """`os/arch` → platform manifest digest, read off the tag's image index."""
    request = urllib.request.Request(
        f"http://{registry}/v2/{repository}/manifests/{tag}",
        headers={"Accept": "application/vnd.oci.image.index.v1+json"},
    )
    with urllib.request.urlopen(request) as response:
        index = json.load(response)
    return {
        f"{entry['platform']['os']}/{entry['platform']['architecture']}": entry["digest"]
        for entry in index["manifests"]
    }


def _write_spec(tmp_path: Path, registry: str, repository: str, dependency: str, asset_server) -> Path:
    for platform in PLATFORMS:
        name = f"tool_{platform.replace('/', '_')}"
        (asset_server.dir / name).write_text("#!/bin/sh\necho tool\n")

    (tmp_path / "metadata.json").write_text(
        json.dumps(
            {
                "type": "bundle",
                "version": 1,
                "dependencies": [{"identifier": dependency, "name": "dep", "visibility": "private"}],
            }
        )
    )
    assets = "\n".join(
        f'        "tool_{p.replace("/", "_")}": "{asset_server.url("tool_" + p.replace("/", "_"))}"'
        for p in PLATFORMS
    )
    patterns = "\n".join(f'  "{p}":\n    - "^tool_{p.replace("/", "_")}$"' for p in PLATFORMS)
    spec = tmp_path / "mirror.yml"
    spec.write_text(
        f"""name: tool
target:
  registry: "{registry}"
  repository: "{repository}"
source:
  type: url_index
  versions:
    "1.0.0":
      assets:
{assets}
assets:
{patterns}
asset_type:
  type: binary
  name: tool
metadata:
  default: metadata.json
build_timestamp: none
"""
    )
    return spec


def test_tag_only_dependency_is_pinned_per_platform_and_does_not_drift(
    mirror: MirrorRunner,
    ocx_binary: Path,
    registry: str,
    unique_mirror_repo: str,
    asset_server,
    tmp_path: Path,
) -> None:
    unique = uuid.uuid4().hex[:8]
    dep_repository = f"deps/dep-{unique}"
    for platform in PLATFORMS:
        push_stub_ocx_package(
            ocx_binary,
            registry,
            f"{dep_repository}:1.0",
            tmp_path / "dep" / platform.replace("/", "_"),
            content=platform.encode(),
            platform=platform,
        )
    leaves = _leaf_digests(registry, dep_repository, "1.0")
    assert set(leaves) == set(PLATFORMS), f"the dependency must carry both platforms: {leaves}"
    assert len(set(leaves.values())) == 2, "the fixture needs distinct per-platform digests"

    dependency = f"{registry}/{dep_repository}:1.0"
    spec = _write_spec(tmp_path, registry, unique_mirror_repo, dependency, asset_server)
    mirror.env["OCX_HOME"] = str(tmp_path / "ocx-home")

    work_dir = tmp_path / "prepare"
    mirror.run(
        "package", "pipeline", "prepare",
        "--spec", str(spec), "--version", "1.0.0", "--work-dir", str(work_dir),
    )
    for platform in PLATFORMS:
        sidecar = json.loads((work_dir / "1.0.0" / platform.replace("/", "_") / "metadata.json").read_text())
        identifiers = [entry["identifier"] for entry in sidecar["dependencies"]]
        assert identifiers == [f"{dependency}@{leaves[platform]}"], (
            f"{platform}: the tag must be pinned to this platform's leaf manifest: {identifiers}"
        )

    # Publish through the in-process leg (the same prepare path), then ask the
    # download-free drift check whether the published pins match the tag-only
    # spec. They were compiled from it, so nothing may read as drifted.
    mirror.run("package", "sync", str(spec), "--work-dir", str(tmp_path / "sync"))
    plan = json.loads(
        mirror.run("package", "pipeline", "plan", "--spec", str(spec), "--format", "json").stdout
    )
    assert not plan["has_drift"], f"a tile published from the spec must be current: {plan['versions']}"
