"""`asset_type: archive` member filters: `include:` allow-list, `exclude:` veto.

`pipeline prepare` lays the archive out, applies the filter after
`strip_components`, and bundles what is left. The bundle is the witness.
"""
from __future__ import annotations

import io
import json
import tarfile
from pathlib import Path

from src.mirror_runner import MirrorRunner

ASSET = "tool.tar.gz"


def _build_asset(asset_server) -> None:
    """`top/{bin/tool, bin/dbg, share/README, LICENSE}` as a gzip tarball."""
    members = {
        "top/bin/tool": (b"#!/bin/sh\necho tool\n", 0o755),
        "top/bin/dbg": (b"debug", 0o644),
        "top/share/README": (b"readme", 0o644),
        "top/LICENSE": (b"license", 0o644),
    }
    with tarfile.open(asset_server.dir / ASSET, "w:gz") as archive:
        for name, (data, mode) in members.items():
            info = tarfile.TarInfo(name)
            info.size = len(data)
            info.mode = mode
            archive.addfile(info, io.BytesIO(data))


def _write_spec(tmp_path: Path, registry: str, repository: str, asset_server, filter_lines: str) -> Path:
    (tmp_path / "metadata.json").write_text(json.dumps({"type": "bundle", "version": 1}))
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
        "{ASSET}": "{asset_server.url(ASSET)}"
assets:
  "linux/amd64":
    - "^tool\\\\.tar\\\\.gz$"
asset_type:
  type: archive
  strip_components: 1
{filter_lines}
metadata:
  default: metadata.json
build_timestamp: none
"""
    )
    return spec


def _prepare(mirror: MirrorRunner, spec: Path, work_dir: Path, *, check: bool = True):
    return mirror.run(
        "package", "pipeline", "prepare",
        "--spec", str(spec), "--version", "1.0.0", "--work-dir", str(work_dir),
        check=check,
    )


def _bundle_files(work_dir: Path) -> set[str]:
    bundle = work_dir / "1.0.0" / "linux_amd64" / "bundle.tar.xz"
    with tarfile.open(bundle, "r:xz") as archive:
        return {member.name.removeprefix("./") for member in archive.getmembers() if member.isfile()}


def test_exclude_vetoes_include_after_strip_components(
    mirror: MirrorRunner, registry: str, unique_mirror_repo: str, asset_server, tmp_path: Path
) -> None:
    _build_asset(asset_server)
    spec = _write_spec(
        tmp_path, registry, unique_mirror_repo, asset_server,
        '  include: ["bin/*", "LICENSE"]\n  exclude: ["bin/dbg"]',
    )
    work_dir = tmp_path / "prepare"
    _prepare(mirror, spec, work_dir)
    assert _bundle_files(work_dir) == {"bin/tool", "LICENSE"}


def test_a_misspelled_filter_key_is_refused(
    mirror: MirrorRunner, registry: str, unique_mirror_repo: str, asset_server, tmp_path: Path
) -> None:
    _build_asset(asset_server)
    spec = _write_spec(
        tmp_path, registry, unique_mirror_repo, asset_server,
        '  include: ["bin/*"]\n  excludes: ["bin/dbg"]',
    )
    result = _prepare(mirror, spec, tmp_path / "prepare", check=False)
    assert result.returncode == 65, "a misspelt key must not publish an unfiltered bundle"
    assert "excludes" in result.stderr, result.stderr


def test_an_include_naming_a_directory_fails_with_the_glob_hint(
    mirror: MirrorRunner, registry: str, unique_mirror_repo: str, asset_server, tmp_path: Path
) -> None:
    _build_asset(asset_server)
    spec = _write_spec(tmp_path, registry, unique_mirror_repo, asset_server, '  include: ["bin"]')
    result = _prepare(mirror, spec, tmp_path / "prepare", check=False)
    assert result.returncode != 0, "an include that matches no file must fail the version"
    assert "bin/*" in result.stderr, result.stderr
