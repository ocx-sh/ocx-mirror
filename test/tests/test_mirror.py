"""Acceptance tests for ocx-mirror."""
from __future__ import annotations

import shutil
import stat
import sys
import tarfile
from pathlib import Path

from src.mirror_runner import MirrorRunner
from src.runner import OcxRunner, current_platform

FIXTURES_DIR = Path(__file__).resolve().parent.parent / "fixtures" / "mirror" / "test-tool"


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _make_tarball(tmp_path: Path, name: str, marker: str) -> Path:
    """Create a .tar.gz containing a bin/<name> script echoing marker."""
    pkg_dir = tmp_path / f"pkg-{name}"
    bin_dir = pkg_dir / "bin"
    bin_dir.mkdir(parents=True, exist_ok=True)

    script = bin_dir / name
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


def _write_spec_yaml(
    path: Path,
    *,
    name: str,
    registry: str,
    repo: str,
    versions: list[dict],
    metadata_path: str,
    cascade: bool = True,
    skip_prereleases: bool = False,
    versions_config: dict | None = None,
    asset_digests: dict[str, str] | None = None,
    verify: dict | None = None,
) -> None:
    """Write a mirror spec YAML file.

    versions: list of dicts with keys "version", "assets" (dict), optional "prerelease".
    The YAML source.versions is a map keyed by version string.

    asset_digests: asset name -> sha256 digest. A named asset is emitted in the
    object form (``{url, sha256}``) instead of the bare URL string.

    verify: emitted verbatim as the spec's ``verify:`` block (scalar values only).

    versions_config: ``min``/``max`` accept a plain string (the shorthand) or a
    dict. A dict emits the nested object form -- ``{"version": "1.0.0",
    "inclusive": True}``, ``{"url": ...}`` or ``{"generator": {...}}``.
    """
    plat = current_platform()
    digests = asset_digests or {}
    lines = [
        f"name: {name}",
        "source:",
        "  type: url_index",
        "  versions:",
    ]
    for v in versions:
        ver = v["version"]
        lines.append(f"    \"{ver}\":")
        lines.append("      assets:")
        for asset_name, url in v["assets"].items():
            if asset_name in digests:
                lines.append(f"        \"{asset_name}\":")
                lines.append(f"          url: \"{url}\"")
                lines.append(f"          sha256: \"{digests[asset_name]}\"")
            else:
                lines.append(f"        \"{asset_name}\": \"{url}\"")
        if v.get("prerelease"):
            lines.append("      prerelease: true")

    lines += [
        "assets:",
        f"  \"{plat}\":",
        f"    - \"^{name}\\\\.tar\\\\.gz$\"",
        "target:",
        f"  registry: \"{registry}\"",
        f"  repository: \"{repo}\"",
        "metadata:",
        f"  default: \"{metadata_path}\"",
        f"cascade: {str(cascade).lower()}",
        "build_timestamp: none",
    ]
    if skip_prereleases:
        lines.append("skip_prereleases: true")
    if verify:
        lines.append("verify:")
        for key, value in verify.items():
            lines.append(f"  {key}: {_yaml_scalar(value)}")
    if versions_config:
        lines.append("versions:")
        for edge in ("min", "max"):
            if edge in versions_config:
                lines.extend(_bound_lines(edge, versions_config[edge]))
        if "new_per_run" in versions_config:
            lines.append(f"  new_per_run: {versions_config['new_per_run']}")

    path.write_text("\n".join(lines) + "\n")


def _yaml_scalar(value) -> str:
    """Render a Python scalar as the YAML the spec parser expects."""
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, (int, float)):
        return str(value)
    return f"\"{value}\""


def _bound_lines(edge: str, bound) -> list[str]:
    """Render one `versions.<edge>` bound: shorthand string or object form."""
    if not isinstance(bound, dict):
        return [f"  {edge}: \"{bound}\""]

    lines = [f"  {edge}:"]
    if "version" in bound:
        lines.append(f"    version: \"{bound['version']}\"")
    elif "url" in bound:
        lines.append("    version:")
        lines.append(f"      url: \"{bound['url']}\"")
    elif "generator" in bound:
        generator = bound["generator"]
        lines.append("    version:")
        lines.append("      generator:")
        lines.append("        command:")
        for arg in generator["command"]:
            lines.append(f"          - \"{arg}\"")
        for key in ("working_directory", "timeout_seconds"):
            if key in generator:
                lines.append(f"        {key}: {_yaml_scalar(generator[key])}")
    if "inclusive" in bound:
        lines.append(f"    inclusive: {_yaml_scalar(bound['inclusive'])}")
    return lines


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


# `mirror_binary`, `mirror`, `unique_mirror_repo` and `asset_server` live in
# `conftest.py` — the pipeline and e2e suites need the same four.


# ---------------------------------------------------------------------------
# Tests: validate command
# ---------------------------------------------------------------------------


def test_validate_valid_spec(mirror: MirrorRunner, tmp_path: Path, registry: str, unique_mirror_repo: str):
    """validate command exits 0 for a valid spec."""
    metadata_path = str(FIXTURES_DIR / "metadata.json")
    spec_path = tmp_path / "mirror-test.yaml"
    _write_spec_yaml(
        spec_path,
        name="test-tool",
        registry=registry,
        repo=unique_mirror_repo,
        versions=[{"version": "1.0.0", "assets": {"test-tool.tar.gz": "https://example.com/test-tool.tar.gz"}}],
        metadata_path=metadata_path,
    )
    result = mirror.run("package", "validate", str(spec_path))
    assert result.returncode == 0


def test_validate_invalid_spec(mirror: MirrorRunner, tmp_path: Path):
    """validate command exits non-zero for an invalid spec."""
    spec_path = tmp_path / "bad-spec.yaml"
    spec_path.write_text("name: test\n")  # Missing required fields
    result = mirror.run("package", "validate", str(spec_path), check=False)
    assert result.returncode != 0


# ---------------------------------------------------------------------------
# Tests: check (dry-run)
# ---------------------------------------------------------------------------


def test_check_shows_would_mirror(
    mirror: MirrorRunner, tmp_path: Path, registry: str,
    unique_mirror_repo: str, asset_server,
):
    """check command lists versions that would be mirrored."""
    tarball = _make_tarball(tmp_path, "test-tool", "marker-check")
    shutil.copy(tarball, asset_server.dir / "test-tool.tar.gz")

    metadata_path = str(FIXTURES_DIR / "metadata.json")
    spec_path = tmp_path / "mirror-test.yaml"
    _write_spec_yaml(
        spec_path,
        name="test-tool",
        registry=registry,
        repo=unique_mirror_repo,
        versions=[
            {"version": "1.0.0", "assets": {"test-tool.tar.gz": asset_server.url("test-tool.tar.gz")}},
            {"version": "2.0.0", "assets": {"test-tool.tar.gz": asset_server.url("test-tool.tar.gz")}},
        ],
        metadata_path=metadata_path,
    )

    result = mirror.run("package", "check", str(spec_path), "--work-dir", str(mirror.temp_dir))
    assert "would mirror" in result.stderr.lower()


# ---------------------------------------------------------------------------
# Tests: pipeline prepare — per-platform version applicability
# ---------------------------------------------------------------------------


def test_pipeline_prepare_drops_excluded_platform(
    mirror: MirrorRunner, tmp_path: Path, registry: str,
    unique_mirror_repo: str, asset_server,
):
    """A platform with a ``broken`` exclude for a version is never prepared.

    Declares two platforms (the host platform and the opposite arch) and marks
    the non-host platform as ``broken`` for version 1.0.0. ``pipeline prepare``
    must produce a bundle for the host platform only — the excluded
    ``(version, platform)`` pair is never resolved, downloaded, or bundled.
    """
    tarball = _make_tarball(tmp_path, "test-tool", "marker-applicability")
    host = current_platform()
    host_os, host_arch = host.split("/")
    other_arch = "arm64" if host_arch == "amd64" else "amd64"
    other = f"{host_os}/{other_arch}"

    host_asset = f"test-tool-{host_arch}.tar.gz"
    other_asset = f"test-tool-{other_arch}.tar.gz"
    shutil.copy(tarball, asset_server.dir / host_asset)
    shutil.copy(tarball, asset_server.dir / other_asset)

    metadata_path = str(FIXTURES_DIR / "metadata.json")
    spec_path = tmp_path / "mirror-applicability.yaml"
    spec_path.write_text(
        "\n".join(
            [
                "name: test-tool",
                "source:",
                "  type: url_index",
                "  versions:",
                '    "1.0.0":',
                "      assets:",
                f'        "{host_asset}": "{asset_server.url(host_asset)}"',
                f'        "{other_asset}": "{asset_server.url(other_asset)}"',
                "assets:",
                f'  "{host}":',
                f"    - '^test-tool-{host_arch}\\.tar\\.gz$'",
                f'  "{other}":',
                f"    - '^test-tool-{other_arch}\\.tar\\.gz$'",
                "target:",
                f'  registry: "{registry}"',
                f'  repository: "{unique_mirror_repo}"',
                "metadata:",
                f'  default: "{metadata_path}"',
                "build_timestamp: none",
                "platforms:",
                f"  {host}:",
                "    runner: ubuntu-latest",
                f"  {other}:",
                "    runner: ubuntu-latest",
                "    exclude:",
                '      - version: "1.0.0"',
                '        reason: "known broken on this release"',
                "        severity: broken",
            ]
        )
        + "\n"
    )

    work_dir = mirror.temp_dir / "prep"
    mirror.run("package", "pipeline", "prepare", "--spec", str(spec_path), "--version", "1.0.0", "--work-dir", str(work_dir))

    version_dir = work_dir / "1.0.0"
    host_slug = host.replace("/", "_")
    other_slug = other.replace("/", "_")
    assert (version_dir / host_slug / "bundle.tar.xz").exists(), (
        f"host platform {host} must be prepared; dir contents: {list(version_dir.iterdir())}"
    )
    assert not (version_dir / other_slug).exists(), (
        f"excluded platform {other} must NOT be prepared (broken exclude for 1.0.0)"
    )


# ---------------------------------------------------------------------------
# Tests: sync — full lifecycle
# ---------------------------------------------------------------------------


def test_sync_mirrors_versions(
    mirror: MirrorRunner, ocx: OcxRunner, tmp_path: Path,
    registry: str, unique_mirror_repo: str, asset_server,
):
    """sync mirrors upstream versions into the OCI registry."""
    tarball = _make_tarball(tmp_path, "test-tool", "marker-sync")
    shutil.copy(tarball, asset_server.dir / "test-tool.tar.gz")

    metadata_path = str(FIXTURES_DIR / "metadata.json")
    spec_path = tmp_path / "mirror-test.yaml"
    _write_spec_yaml(
        spec_path,
        name="test-tool",
        registry=registry,
        repo=unique_mirror_repo,
        versions=[
            {"version": "1.0.0", "assets": {"test-tool.tar.gz": asset_server.url("test-tool.tar.gz")}},
            {"version": "2.0.0", "assets": {"test-tool.tar.gz": asset_server.url("test-tool.tar.gz")}},
        ],
        metadata_path=metadata_path,
    )

    result = mirror.run("package", "sync", str(spec_path), "--work-dir", str(mirror.temp_dir))
    assert "pushed" in result.stdout.lower() or "mirror complete" in result.stderr.lower()

    # Verify tags exist via ocx
    ocx.plain("index", "update", unique_mirror_repo)
    data = ocx.json("index", "list", unique_mirror_repo)
    tags = data[unique_mirror_repo]
    assert "1.0.0" in tags
    assert "2.0.0" in tags


def test_sync_idempotent(
    mirror: MirrorRunner, tmp_path: Path,
    registry: str, unique_mirror_repo: str, asset_server,
):
    """Re-running sync with same versions does nothing new."""
    tarball = _make_tarball(tmp_path, "test-tool", "marker-idem")
    shutil.copy(tarball, asset_server.dir / "test-tool.tar.gz")

    metadata_path = str(FIXTURES_DIR / "metadata.json")
    spec_path = tmp_path / "mirror-test.yaml"
    _write_spec_yaml(
        spec_path,
        name="test-tool",
        registry=registry,
        repo=unique_mirror_repo,
        versions=[
            {"version": "1.0.0", "assets": {"test-tool.tar.gz": asset_server.url("test-tool.tar.gz")}},
        ],
        metadata_path=metadata_path,
    )

    # First sync
    mirror.run("package", "sync", str(spec_path), "--work-dir", str(mirror.temp_dir))

    # Second sync — should find nothing new
    result = mirror.run("package", "sync", str(spec_path), "--work-dir", str(mirror.temp_dir))
    assert "nothing to mirror" in result.stderr.lower()


# ---------------------------------------------------------------------------
# Tests: sync — cascade
# ---------------------------------------------------------------------------


def test_sync_cascade_creates_rolling_tags(
    mirror: MirrorRunner, ocx: OcxRunner, tmp_path: Path,
    registry: str, unique_mirror_repo: str, asset_server,
):
    """sync with cascade creates rolling tags (1.2, 1, latest)."""
    tarball = _make_tarball(tmp_path, "test-tool", "marker-cascade")
    shutil.copy(tarball, asset_server.dir / "test-tool.tar.gz")

    metadata_path = str(FIXTURES_DIR / "metadata.json")
    spec_path = tmp_path / "mirror-test.yaml"
    _write_spec_yaml(
        spec_path,
        name="test-tool",
        registry=registry,
        repo=unique_mirror_repo,
        versions=[
            {"version": "1.2.3", "assets": {"test-tool.tar.gz": asset_server.url("test-tool.tar.gz")}},
        ],
        metadata_path=metadata_path,
        cascade=True,
    )

    mirror.run("package", "sync", str(spec_path), "--work-dir", str(mirror.temp_dir))

    # Verify rolling tags
    ocx.plain("index", "update", unique_mirror_repo)
    data = ocx.json("index", "list", unique_mirror_repo)
    tags = data[unique_mirror_repo]
    for expected in ["1.2.3", "1.2", "1", "latest"]:
        assert expected in tags, f"Expected tag '{expected}' in {tags}"


# ---------------------------------------------------------------------------
# Tests: sync — version filtering
# ---------------------------------------------------------------------------


def test_sync_version_min_filter(
    mirror: MirrorRunner, ocx: OcxRunner, tmp_path: Path,
    registry: str, unique_mirror_repo: str, asset_server,
):
    """sync with min version filter skips versions below threshold."""
    tarball = _make_tarball(tmp_path, "test-tool", "marker-min")
    shutil.copy(tarball, asset_server.dir / "test-tool.tar.gz")

    metadata_path = str(FIXTURES_DIR / "metadata.json")
    spec_path = tmp_path / "mirror-test.yaml"
    _write_spec_yaml(
        spec_path,
        name="test-tool",
        registry=registry,
        repo=unique_mirror_repo,
        versions=[
            {"version": "1.0.0", "assets": {"test-tool.tar.gz": asset_server.url("test-tool.tar.gz")}},
            {"version": "2.0.0", "assets": {"test-tool.tar.gz": asset_server.url("test-tool.tar.gz")}},
        ],
        metadata_path=metadata_path,
        versions_config={"min": "1.1.0"},
        cascade=False,
    )

    mirror.run("package", "sync", str(spec_path), "--work-dir", str(mirror.temp_dir))

    ocx.plain("index", "update", f"{unique_mirror_repo}:2.0.0")
    data = ocx.json("index", "list", unique_mirror_repo)
    tags = data[unique_mirror_repo]
    assert "2.0.0" in tags
    assert "1.0.0" not in tags


def test_sync_new_per_run_cap(
    mirror: MirrorRunner, ocx: OcxRunner, tmp_path: Path,
    registry: str, unique_mirror_repo: str, asset_server,
):
    """sync with new_per_run limits versions mirrored per invocation."""
    tarball = _make_tarball(tmp_path, "test-tool", "marker-cap")
    shutil.copy(tarball, asset_server.dir / "test-tool.tar.gz")

    metadata_path = str(FIXTURES_DIR / "metadata.json")
    spec_path = tmp_path / "mirror-test.yaml"
    _write_spec_yaml(
        spec_path,
        name="test-tool",
        registry=registry,
        repo=unique_mirror_repo,
        versions=[
            {"version": "1.0.0", "assets": {"test-tool.tar.gz": asset_server.url("test-tool.tar.gz")}},
            {"version": "2.0.0", "assets": {"test-tool.tar.gz": asset_server.url("test-tool.tar.gz")}},
            {"version": "3.0.0", "assets": {"test-tool.tar.gz": asset_server.url("test-tool.tar.gz")}},
        ],
        metadata_path=metadata_path,
        versions_config={"new_per_run": 1},
        cascade=False,
    )

    # First run: should mirror only 1 version
    result = mirror.run("package", "sync", str(spec_path), "--work-dir", str(mirror.temp_dir))
    assert "pushed" in result.stdout.lower()

    # Second run should still have work to do
    result2 = mirror.run("package", "sync", str(spec_path), "--work-dir", str(mirror.temp_dir))
    assert "nothing to mirror" not in result2.stderr.lower()


def test_sync_resolved_max_from_url_is_inclusive(
    mirror: MirrorRunner, ocx: OcxRunner, tmp_path: Path,
    registry: str, unique_mirror_repo: str, asset_server,
):
    """A `max` fetched from a vendor pointer keeps the version it names.

    `inclusive: true` is the whole point of the object form: a channel pointer
    names the release it wants mirrored, not the first one it does not.
    """
    tarball = _make_tarball(tmp_path, "test-tool", "marker-url-max")
    shutil.copy(tarball, asset_server.dir / "test-tool.tar.gz")
    (asset_server.dir / "stable").write_text("2.0.0\n")

    metadata_path = str(FIXTURES_DIR / "metadata.json")
    spec_path = tmp_path / "mirror-test.yaml"
    _write_spec_yaml(
        spec_path,
        name="test-tool",
        registry=registry,
        repo=unique_mirror_repo,
        versions=[
            {"version": "2.0.0", "assets": {"test-tool.tar.gz": asset_server.url("test-tool.tar.gz")}},
            {"version": "3.0.0", "assets": {"test-tool.tar.gz": asset_server.url("test-tool.tar.gz")}},
        ],
        metadata_path=metadata_path,
        versions_config={"max": {"url": asset_server.url("stable"), "inclusive": True}},
        cascade=False,
    )

    mirror.run("package", "sync", str(spec_path), "--work-dir", str(mirror.temp_dir))

    ocx.plain("index", "update", f"{unique_mirror_repo}:2.0.0")
    tags = ocx.json("index", "list", unique_mirror_repo)[unique_mirror_repo]
    assert "2.0.0" in tags, f"an inclusive ceiling mirrors the version it names: {tags}"
    assert "3.0.0" not in tags, f"nothing above the ceiling: {tags}"


def test_sync_resolved_max_from_generator_is_exclusive_when_declared(
    mirror: MirrorRunner, ocx: OcxRunner, tmp_path: Path,
    registry: str, unique_mirror_repo: str, asset_server,
):
    """A generated `max` with `inclusive: false` drops the version it names."""
    tarball = _make_tarball(tmp_path, "test-tool", "marker-gen-max")
    shutil.copy(tarball, asset_server.dir / "test-tool.tar.gz")

    metadata_path = str(FIXTURES_DIR / "metadata.json")
    spec_path = tmp_path / "mirror-test.yaml"
    _write_spec_yaml(
        spec_path,
        name="test-tool",
        registry=registry,
        repo=unique_mirror_repo,
        versions=[
            {"version": "1.0.0", "assets": {"test-tool.tar.gz": asset_server.url("test-tool.tar.gz")}},
            {"version": "2.0.0", "assets": {"test-tool.tar.gz": asset_server.url("test-tool.tar.gz")}},
        ],
        metadata_path=metadata_path,
        versions_config={
            "max": {"generator": {"command": ["sh", "-c", "echo 2.0.0"]}, "inclusive": False},
        },
        cascade=False,
    )

    mirror.run("package", "sync", str(spec_path), "--work-dir", str(mirror.temp_dir))

    ocx.plain("index", "update", f"{unique_mirror_repo}:1.0.0")
    tags = ocx.json("index", "list", unique_mirror_repo)[unique_mirror_repo]
    assert "1.0.0" in tags, f"everything below the ceiling is mirrored: {tags}"
    assert "2.0.0" not in tags, f"an exclusive ceiling drops the version it names: {tags}"


def test_sync_resolved_min_from_url_is_exclusive_when_declared(
    mirror: MirrorRunner, ocx: OcxRunner, tmp_path: Path,
    registry: str, unique_mirror_repo: str, asset_server,
):
    """The lower edge's new degree of freedom, end to end.

    Before the object form, a `min` was always inclusive; `inclusive: false`
    is only expressible through the long spelling.
    """
    tarball = _make_tarball(tmp_path, "test-tool", "marker-url-min")
    shutil.copy(tarball, asset_server.dir / "test-tool.tar.gz")
    (asset_server.dir / "floor").write_text("1.0.0\n")

    metadata_path = str(FIXTURES_DIR / "metadata.json")
    spec_path = tmp_path / "mirror-test.yaml"
    _write_spec_yaml(
        spec_path,
        name="test-tool",
        registry=registry,
        repo=unique_mirror_repo,
        versions=[
            {"version": "1.0.0", "assets": {"test-tool.tar.gz": asset_server.url("test-tool.tar.gz")}},
            {"version": "2.0.0", "assets": {"test-tool.tar.gz": asset_server.url("test-tool.tar.gz")}},
        ],
        metadata_path=metadata_path,
        versions_config={"min": {"url": asset_server.url("floor"), "inclusive": False}},
        cascade=False,
    )

    mirror.run("package", "sync", str(spec_path), "--work-dir", str(mirror.temp_dir))

    ocx.plain("index", "update", f"{unique_mirror_repo}:2.0.0")
    tags = ocx.json("index", "list", unique_mirror_repo)[unique_mirror_repo]
    assert "2.0.0" in tags, f"above an exclusive floor is mirrored: {tags}"
    assert "1.0.0" not in tags, f"an exclusive floor drops the version it names: {tags}"


def test_sync_unreachable_max_url_fails_the_run(
    mirror: MirrorRunner, tmp_path: Path,
    registry: str, unique_mirror_repo: str, asset_server,
):
    """Fail-closed: a ceiling that cannot be resolved aborts, never widens.

    The alternative — falling back to an unbounded window — mirrors every
    release the pointer exists to hold back. The asset server's request log is
    what makes "nothing was published" an assertion: the run never reached a
    download, so the target was never written to.
    """
    tarball = _make_tarball(tmp_path, "test-tool", "marker-unreachable")
    shutil.copy(tarball, asset_server.dir / "test-tool.tar.gz")

    metadata_path = str(FIXTURES_DIR / "metadata.json")
    spec_path = tmp_path / "mirror-test.yaml"
    _write_spec_yaml(
        spec_path,
        name="test-tool",
        registry=registry,
        repo=unique_mirror_repo,
        versions=[
            {"version": "1.0.0", "assets": {"test-tool.tar.gz": asset_server.url("test-tool.tar.gz")}},
            {"version": "2.0.0", "assets": {"test-tool.tar.gz": asset_server.url("test-tool.tar.gz")}},
        ],
        metadata_path=metadata_path,
        versions_config={"max": {"url": asset_server.url("missing"), "inclusive": True}},
        cascade=False,
    )

    result = mirror.run(
        "package", "sync", str(spec_path), "--work-dir", str(mirror.temp_dir), check=False
    )

    assert result.returncode == 69, (
        f"an unresolvable bound is an unusable source (69), got {result.returncode}\n{result.stderr}"
    )
    assert "versions.max" in result.stderr, f"the message must name the edge: {result.stderr}"
    downloads = [r for r in asset_server.requests if "test-tool.tar.gz" in r]
    assert downloads == [], f"a failed resolve must publish nothing: {asset_server.requests}"
