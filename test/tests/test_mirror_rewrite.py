"""Acceptance tests for `source.url_rewrite` and `OCX_MIRROR_URL_REWRITE` (#75).

The rewrite is a prefix substitution applied at one seam — the tail of
`list_upstream_versions` — so `sync`, `check`, `pipeline plan` and a
`--plan`-less `prepare` all agree, and `plan.json` carries already-rewritten
URLs.

Two hosts out of a one-server fixture: the same tarball is served under two
path prefixes, `/direct/` and `/proxy/`, and `asset_server.requests` makes
"which path was actually fetched" an assertion rather than an inference.

The last test is the GitHub-digest round trip. No GitHub stand-in exists in
this harness, so the plan document is hand-written at `schema_version: 4` —
which is exactly what proves `prepare --plan` honours a digest it never
crawled for itself.
"""
from __future__ import annotations

import hashlib
import json
import shutil
import stat
import sys
import tarfile
from pathlib import Path

from src.mirror_runner import MirrorRunner
from src.runner import current_platform

FIXTURES_DIR = Path(__file__).resolve().parent.parent / "fixtures" / "mirror" / "test-tool"

ASSET = "test-tool.tar.gz"
WRONG_SHA256 = "0" * 64


def _make_tarball(tmp_path: Path, marker: str) -> Path:
    """A .tar.gz holding `bin/test-tool` that echoes `marker`."""
    bin_dir = tmp_path / f"pkg-{marker}" / "bin"
    bin_dir.mkdir(parents=True, exist_ok=True)

    script = bin_dir / "test-tool"
    if sys.platform == "win32":
        script = script.with_suffix(".bat")
        script.write_text(f"@echo {marker}\n")
    else:
        script.write_text(f"#!/bin/sh\necho {marker}\n")
        script.chmod(script.stat().st_mode | stat.S_IEXEC)

    tarball = tmp_path / f"{marker}.tar.gz"
    with tarfile.open(tarball, "w:gz") as tar:
        tar.add(bin_dir, arcname="bin")
    return tarball


def _serve_under_both_prefixes(tmp_path: Path, asset_server, marker: str) -> str:
    """Serve one tarball at `/direct/<ASSET>` and `/proxy/<ASSET>`.

    Two path prefixes stand in for two hosts: the rewrite is a plain string
    substitution on the URL, so a prefix it can match is all the feature needs
    to be exercised end to end.
    """
    tarball = _make_tarball(tmp_path, marker)
    for prefix in ("direct", "proxy"):
        target = asset_server.dir / prefix
        target.mkdir(exist_ok=True)
        shutil.copy(tarball, target / ASSET)
    return hashlib.sha256((asset_server.dir / "direct" / ASSET).read_bytes()).hexdigest()


def _write_spec(path: Path, *, registry: str, repo: str, url: str, url_rewrite: tuple[str, str] | None) -> None:
    """A minimal inline url_index spec, optionally carrying `source.url_rewrite`."""
    lines = [
        "name: test-tool",
        "source:",
        "  type: url_index",
        "  versions:",
        '    "1.0.0":',
        "      assets:",
        f'        "{ASSET}": "{url}"',
    ]
    if url_rewrite is not None:
        source_from, source_to = url_rewrite
        lines += [
            "  url_rewrite:",
            f'    from: "{source_from}"',
            f'    to: "{source_to}"',
        ]
    lines += [
        "assets:",
        f'  "{current_platform()}":',
        '    - "^test-tool\\\\.tar\\\\.gz$"',
        "target:",
        f'  registry: "{registry}"',
        f'  repository: "{repo}"',
        "metadata:",
        f'  default: "{FIXTURES_DIR / "metadata.json"}"',
        "cascade: false",
        "build_timestamp: none",
    ]
    path.write_text("\n".join(lines) + "\n")


def _asset_gets(asset_server) -> list[str]:
    """Only the upstream asset reads — the publish leg records its own entries."""
    return [entry for entry in asset_server.requests if ASSET in entry]


def test_url_rewrite_from_spec(
    mirror: MirrorRunner, tmp_path: Path, registry: str,
    unique_mirror_repo: str, asset_server,
):
    """The spec's own `url_rewrite` moves the download off the declared prefix."""
    _serve_under_both_prefixes(tmp_path, asset_server, "spec-rewrite")

    spec_path = tmp_path / "mirror-test.yaml"
    _write_spec(
        spec_path,
        registry=registry,
        repo=unique_mirror_repo,
        url=asset_server.url(f"direct/{ASSET}"),
        url_rewrite=(f"{asset_server.base_url}/direct/", f"{asset_server.base_url}/proxy/"),
    )

    mirror.run("package", "sync", str(spec_path), "--work-dir", str(mirror.temp_dir))

    fetched = _asset_gets(asset_server)
    assert any(f"/proxy/{ASSET}" in entry for entry in fetched), fetched
    assert not any(f"/direct/{ASSET}" in entry for entry in fetched), fetched


def test_url_rewrite_from_env_overrides_spec(
    mirror: MirrorRunner, tmp_path: Path, registry: str,
    unique_mirror_repo: str, asset_server,
):
    """`OCX_MIRROR_URL_REWRITE` replaces the spec's block entirely, not partly.

    This is what lets one `mirror.yml` stay byte-identical between the public
    contrib repository and an internal fork — `extends:`' shallow top-level
    merge cannot contribute `source.url_rewrite` alone.
    """
    _serve_under_both_prefixes(tmp_path, asset_server, "env-rewrite")

    spec_path = tmp_path / "mirror-test.yaml"
    # The spec points somewhere that serves nothing; only the env target does.
    _write_spec(
        spec_path,
        registry=registry,
        repo=unique_mirror_repo,
        url=asset_server.url(f"direct/{ASSET}"),
        url_rewrite=(f"{asset_server.base_url}/direct/", f"{asset_server.base_url}/nowhere/"),
    )
    mirror.env["OCX_MIRROR_URL_REWRITE"] = f"{asset_server.base_url}/direct/={asset_server.base_url}/proxy/"

    mirror.run("package", "sync", str(spec_path), "--work-dir", str(mirror.temp_dir))

    fetched = _asset_gets(asset_server)
    assert any(f"/proxy/{ASSET}" in entry for entry in fetched), fetched
    assert not any(f"/nowhere/{ASSET}" in entry for entry in fetched), fetched


def test_url_rewrite_malformed_env_is_refused(
    mirror: MirrorRunner, tmp_path: Path, registry: str,
    unique_mirror_repo: str, asset_server,
):
    """A malformed variable exits 64 and names the variable, never its value."""
    _serve_under_both_prefixes(tmp_path, asset_server, "bad-env")

    spec_path = tmp_path / "mirror-test.yaml"
    _write_spec(
        spec_path,
        registry=registry,
        repo=unique_mirror_repo,
        url=asset_server.url(f"direct/{ASSET}"),
        url_rewrite=None,
    )
    mirror.env["OCX_MIRROR_URL_REWRITE"] = "nonsense-no-separator"

    result = mirror.run("package", "sync", str(spec_path), "--work-dir", str(mirror.temp_dir), check=False)
    assert result.returncode == 64, f"rc={result.returncode}\n{result.stderr}"
    output = result.stdout + result.stderr
    assert "OCX_MIRROR_URL_REWRITE" in output, output
    assert "nonsense-no-separator" not in output, output


def test_github_digest_survives_prepare_plan(
    mirror: MirrorRunner, tmp_path: Path, registry: str,
    unique_mirror_repo: str, asset_server,
):
    """A `digest` in `plan.json` is honoured by `prepare --plan`.

    The round trip the GitHub half of #76 depends on: `plan` stamps the
    Releases API's declared digest onto the asset entry, and the prepare leg —
    a different process, on a different runner — verifies against it without
    ever crawling the source. Hand-written because no GitHub mock exists;
    schema_version 4 is the contract under test.
    """
    real_digest = _serve_under_both_prefixes(tmp_path, asset_server, "plan-digest")

    spec_path = tmp_path / "mirror-test.yaml"
    _write_spec(
        spec_path,
        registry=registry,
        repo=unique_mirror_repo,
        url=asset_server.url(f"direct/{ASSET}"),
        url_rewrite=None,
    )

    def run_with(digest: str):
        plan_path = tmp_path / f"plan-{digest[:8]}.json"
        plan_path.write_text(json.dumps({
            "schema_version": 4,
            "has_new": True,
            "has_drift": False,
            "versions": [{
                "version": "1.0.0",
                "source_version": "1.0.0",
                "platforms": [current_platform()],
                "kind": "new",
                "assets": [{
                    "platform": current_platform(),
                    "asset_name": ASSET,
                    "url": asset_server.url(f"direct/{ASSET}"),
                    "digest": f"sha256:{digest}",
                }],
            }],
            "target": f"{registry}/{unique_mirror_repo}",
            "ocx_mirror_rev": None,
        }))
        return mirror.run(
            "package", "pipeline", "prepare",
            "--spec", str(spec_path),
            "--version", "1.0.0",
            "--work-dir", str(mirror.temp_dir / digest[:8]),
            "--plan", str(plan_path),
            check=False,
        )

    wrong = run_with(WRONG_SHA256)
    assert wrong.returncode != 0, wrong.stdout
    assert "digest mismatch" in (wrong.stdout + wrong.stderr).lower()

    right = run_with(real_digest)
    assert right.returncode == 0, f"rc={right.returncode}\n{right.stderr}"
