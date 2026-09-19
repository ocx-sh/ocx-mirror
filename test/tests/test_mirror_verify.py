"""Acceptance tests for `verify:`'s publisher-declared digest policies (#76).

`verify.github_asset_digest` verified nothing before this release: the
orchestrator handed `verify()` an empty map and `VersionInfo` carried no
digest at all. These tests drive the url_index half of the now-live carrier
end to end — the object asset form `{url, sha256}`, the three-state
`url_index_digest` policy, and the regression guard that a v1 document's bare
string assets still work.

The GitHub half has no mock (no GitHub stand-in exists in this harness); its
round trip through `plan.json` is pinned in `test_mirror_rewrite.py`.
"""
from __future__ import annotations

import hashlib
import json
import shutil
import urllib.error
import urllib.request
from pathlib import Path

from src.mirror_runner import MirrorRunner
from test_mirror import FIXTURES_DIR, _make_tarball, _write_spec_yaml

WRONG_SHA256 = "0" * 64


def _sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def _tags(registry: str, repository: str) -> frozenset[str]:
    """The repository's tag list, empty when it was never created."""
    try:
        with urllib.request.urlopen(f"http://{registry}/v2/{repository}/tags/list") as resp:
            return frozenset(json.load(resp)["tags"] or [])
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return frozenset()
        raise


def _serve_tarball(tmp_path: Path, asset_server, marker: str) -> str:
    """Put a tarball on the asset server; return its real sha256."""
    tarball = _make_tarball(tmp_path, "test-tool", marker)
    served = asset_server.dir / "test-tool.tar.gz"
    shutil.copy(tarball, served)
    return _sha256(served)


def test_url_index_object_asset_digest_verifies(
    mirror: MirrorRunner, tmp_path: Path, registry: str,
    unique_mirror_repo: str, asset_server,
):
    """An object asset declaring the served bytes' real sha256 publishes."""
    digest = _serve_tarball(tmp_path, asset_server, "marker-digest-ok")

    spec_path = tmp_path / "mirror-test.yaml"
    _write_spec_yaml(
        spec_path,
        name="test-tool",
        registry=registry,
        repo=unique_mirror_repo,
        versions=[{"version": "1.0.0", "assets": {"test-tool.tar.gz": asset_server.url("test-tool.tar.gz")}}],
        metadata_path=str(FIXTURES_DIR / "metadata.json"),
        asset_digests={"test-tool.tar.gz": digest},
    )

    mirror.run("package", "sync", str(spec_path), "--work-dir", str(mirror.temp_dir))
    assert "1.0.0" in _tags(registry, unique_mirror_repo)


def test_url_index_digest_mismatch_hard_fails(
    mirror: MirrorRunner, tmp_path: Path, registry: str,
    unique_mirror_repo: str, asset_server,
):
    """A declared digest the bytes do not match reds the run and publishes nothing.

    This is the check that makes a rewritten download host safe (#75): the
    digest is compared against local bytes, so it proves the proxy served what
    the publisher declared.
    """
    _serve_tarball(tmp_path, asset_server, "marker-digest-bad")

    spec_path = tmp_path / "mirror-test.yaml"
    _write_spec_yaml(
        spec_path,
        name="test-tool",
        registry=registry,
        repo=unique_mirror_repo,
        versions=[{"version": "1.0.0", "assets": {"test-tool.tar.gz": asset_server.url("test-tool.tar.gz")}}],
        metadata_path=str(FIXTURES_DIR / "metadata.json"),
        asset_digests={"test-tool.tar.gz": WRONG_SHA256},
    )

    result = mirror.run("package", "sync", str(spec_path), "--work-dir", str(mirror.temp_dir), check=False)
    assert result.returncode != 0, result.stdout
    assert "digest mismatch" in (result.stdout + result.stderr).lower()
    assert _tags(registry, unique_mirror_repo) == frozenset()


def test_url_index_digest_require_fails_without_digest(
    mirror: MirrorRunner, tmp_path: Path, registry: str,
    unique_mirror_repo: str, asset_server,
):
    """`require` reds a string-form asset — the guard against a silent typo.

    A `sha265:` misspelling in a generator degrades the object form to the
    string form, which `if_present` accepts happily. `require` is how an
    operator says that degradation is a failure.
    """
    _serve_tarball(tmp_path, asset_server, "marker-require")

    spec_path = tmp_path / "mirror-test.yaml"
    _write_spec_yaml(
        spec_path,
        name="test-tool",
        registry=registry,
        repo=unique_mirror_repo,
        versions=[{"version": "1.0.0", "assets": {"test-tool.tar.gz": asset_server.url("test-tool.tar.gz")}}],
        metadata_path=str(FIXTURES_DIR / "metadata.json"),
        verify={"url_index_digest": "require"},
    )

    result = mirror.run("package", "sync", str(spec_path), "--work-dir", str(mirror.temp_dir), check=False)
    assert result.returncode != 0, result.stdout
    assert "require" in (result.stdout + result.stderr)
    assert _tags(registry, unique_mirror_repo) == frozenset()


def test_url_index_digest_if_present_allows_missing(
    mirror: MirrorRunner, tmp_path: Path, registry: str,
    unique_mirror_repo: str, asset_server,
):
    """The same string-form asset publishes under the default policy."""
    _serve_tarball(tmp_path, asset_server, "marker-if-present")

    spec_path = tmp_path / "mirror-test.yaml"
    _write_spec_yaml(
        spec_path,
        name="test-tool",
        registry=registry,
        repo=unique_mirror_repo,
        versions=[{"version": "1.0.0", "assets": {"test-tool.tar.gz": asset_server.url("test-tool.tar.gz")}}],
        metadata_path=str(FIXTURES_DIR / "metadata.json"),
        verify={"url_index_digest": "if_present"},
    )

    mirror.run("package", "sync", str(spec_path), "--work-dir", str(mirror.temp_dir))
    assert "1.0.0" in _tags(registry, unique_mirror_repo)


def test_string_asset_form_still_works(
    mirror: MirrorRunner, tmp_path: Path, registry: str,
    unique_mirror_repo: str, asset_server,
):
    """A url-index v1 document with no `verify:` at all keeps mirroring.

    The untagged `IndexAsset` enum's regression guard: every generator in the
    fleet emits the bare-string form, and none of them is being regenerated.
    """
    _serve_tarball(tmp_path, asset_server, "marker-string-form")

    spec_path = tmp_path / "mirror-test.yaml"
    _write_spec_yaml(
        spec_path,
        name="test-tool",
        registry=registry,
        repo=unique_mirror_repo,
        versions=[{"version": "1.0.0", "assets": {"test-tool.tar.gz": asset_server.url("test-tool.tar.gz")}}],
        metadata_path=str(FIXTURES_DIR / "metadata.json"),
    )

    mirror.run("package", "sync", str(spec_path), "--work-dir", str(mirror.temp_dir))
    assert "1.0.0" in _tags(registry, unique_mirror_repo)
