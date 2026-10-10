"""The `pipeline` legs that parse what a spawned `ocx` prints or exits with.

Each test here drives one such leg end to end through the real `ocx` binary
(`OCX_COMMAND`), so a change in `ocx`'s output or exit codes reds at ocx's
`task satellite:contract` instead of at a mirror's next live run. The
`ocx_contract_site` marker names the spawn site a test pins; the ocx side
counts distinct passed ids and reds below its floor, and a skip counts as red.
The remaining sites live beside their legs: `create` in
`test_mirror_pipeline.py`, `push` in `test_mirror_e2e.py`, `lock_derive` in
`test_mirror_pypi.py`.
"""
from __future__ import annotations

import importlib
import json
import subprocess
import sys
import threading
from collections.abc import Iterator
from pathlib import Path

import pytest

from src.helpers import PROJECT_ROOT, fetch_manifest, push_stub_ocx_package
from src.mirror_runner import MirrorRunner

OCX_TESTS_DIR = PROJECT_ROOT / "external" / "ocx" / "test" / "tests"


def _digest(registry: str, repository: str, tag: str) -> str:
    return fetch_manifest(registry, repository, tag)[0]


def _push_versions(ocx_binary: Path, registry: str, repository: str, work: Path) -> None:
    """Two plain version tags, no rolling aliases: the state a push that died
    before its cascade leaves behind."""
    for version in ("1.0.0", "1.1.0"):
        push_stub_ocx_package(
            ocx_binary, registry, f"{repository}:{version}", work / version, content=version.encode()
        )


@pytest.fixture()
def isolated_ocx_home(mirror: MirrorRunner, tmp_path: Path) -> Path:
    home = tmp_path / "ocx-home"
    home.mkdir()
    mirror.env["OCX_HOME"] = str(home)
    return home


# ---------------------------------------------------------------------------
# cascade: `ocx package cascade repair`'s exit 65 is an audit result, not a failure
# ---------------------------------------------------------------------------


@pytest.mark.ocx_contract_site("cascade")
def test_pipeline_cascade_reports_then_repairs_the_aliases_a_partial_push_left(
    mirror: MirrorRunner,
    ocx_binary: Path,
    registry: str,
    unique_mirror_repo: str,
    pipeline_spec: Path,
    isolated_ocx_home: Path,
    tmp_path: Path,
) -> None:
    _push_versions(ocx_binary, registry, unique_mirror_repo, tmp_path / "push")

    audit = mirror.run("package", "pipeline", "cascade", "--spec", str(pipeline_spec), "--dry-run", check=False)
    assert audit.returncode == 65, (
        f"a non-empty repair plan is the audit exit 65, got {audit.returncode}\nstderr: {audit.stderr}"
    )

    mirror.run("package", "pipeline", "cascade", "--spec", str(pipeline_spec))

    newest = _digest(registry, unique_mirror_repo, "1.1.0")
    assert _digest(registry, unique_mirror_repo, "latest") == newest
    assert _digest(registry, unique_mirror_repo, "1") == newest
    assert _digest(registry, unique_mirror_repo, "1.0") == _digest(registry, unique_mirror_repo, "1.0.0")

    healed = mirror.run("package", "pipeline", "cascade", "--spec", str(pipeline_spec), "--dry-run", check=False)
    assert healed.returncode == 0, f"a repaired cascade audits clean: {healed.stderr}"


# ---------------------------------------------------------------------------
# describe: `ocx package description push` publishes the catalog page
# ---------------------------------------------------------------------------


@pytest.mark.ocx_contract_site("describe")
def test_pipeline_describe_publishes_the_catalog_description(
    mirror: MirrorRunner,
    ocx_binary: Path,
    registry: str,
    unique_mirror_repo: str,
    pipeline_spec: Path,
    isolated_ocx_home: Path,
    tmp_path: Path,
) -> None:
    _push_versions(ocx_binary, registry, unique_mirror_repo, tmp_path / "push")
    (pipeline_spec.parent / "CATALOG.md").write_text(
        "---\ntitle: shfmt\ndescription: A shell formatter\nkeywords: [shell]\n---\n\n# shfmt\n"
    )

    mirror.run("package", "pipeline", "describe", "--spec", str(pipeline_spec))

    # The reserved tag is the registry's witness that the description landed.
    assert _digest(registry, unique_mirror_repo, "__ocx.desc")


# ---------------------------------------------------------------------------
# announce: `ocx package announce --format json` is read back as a report
# ---------------------------------------------------------------------------


@pytest.fixture()
def fake_forge() -> Iterator[object]:
    """ocx's own fake GitHub forge, loaded from the pinned `external/ocx` tree.

    One implementation of the forge surface `ocx package announce` speaks, so
    the mirror does not carry a second copy to keep in step. The module
    imports two siblings by bare name, hence the temporary `sys.path` entry.
    """
    sys.path.insert(0, str(OCX_TESTS_DIR))
    try:
        server = importlib.import_module("fake_forge").FakeForge()
    finally:
        sys.path.remove(str(OCX_TESTS_DIR))
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield server
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


@pytest.mark.ocx_contract_site("announce")
def test_pipeline_announce_curates_the_registry_tags_into_the_index(
    mirror: MirrorRunner,
    ocx_binary: Path,
    registry: str,
    unique_mirror_repo: str,
    pipeline_spec: Path,
    isolated_ocx_home: Path,
    fake_forge,
    tmp_path: Path,
) -> None:
    # `__OCX_TESTING_FORGE_BASE_URL` is read only by an `ocx` built with the
    # `__testing` feature, whose `ocx version` reports the placeholder `test`
    # channel; any other build would announce to the real api.github.com.
    version = subprocess.run(
        [str(ocx_binary), "--format", "json", "version"], capture_output=True, text=True
    )
    channel = json.loads(version.stdout).get("channel") if version.returncode == 0 else None
    if channel != "test":
        pytest.skip(
            f"{ocx_binary} is not an ocx `__testing` build (`ocx version` channel: {channel!r}), "
            "so it ignores the fake-forge seam"
        )
    _push_versions(ocx_binary, registry, unique_mirror_repo, tmp_path / "push")
    package = f"acme/{unique_mirror_repo}"
    pipeline_spec.write_text(
        pipeline_spec.read_text()
        + f"announce:\n  package: {package}\n  fork: forkuser/index\n  index_repo: ocx-sh/index\n"
    )
    # The committed root the announce unions the registry's tags onto.
    fake_forge.seed_root(
        "ocx-sh",
        "index",
        f"p/{package}.json",
        {"name": f"{registry}/{package}", "repository": f"oci://{registry}/{unique_mirror_repo}", "tags": {}},
    )
    # The registry is a loopback address: ocx refuses it as an SSRF target
    # unless the config names it, the one escape hatch it has.
    (isolated_ocx_home / "config.toml").write_text(
        f'[registries."{registry}"]\ntrusted_hosts = ["{registry.split(":", 1)[0]}"]\n'
    )
    mirror.env.update(
        {
            "OCX_DEFAULT_REGISTRY": registry,
            "OCX_ANNOUNCE_TOKEN": "announce-test-token",
            "__OCX_TESTING_FORGE_BASE_URL": fake_forge.base_url,
        }
    )

    mirror.run("package", "pipeline", "announce", "--spec", str(pipeline_spec))

    branch = f"indexbot-announce-{package.replace('/', '-')}"
    raw = fake_forge.read_file("forkuser", "index", f"p/{package}.json", branch=branch)
    assert raw is not None, "the announce must commit the rebuilt root to the fork branch"
    assert {"1.0.0", "1.1.0"} <= set(json.loads(raw)["tags"]), raw
