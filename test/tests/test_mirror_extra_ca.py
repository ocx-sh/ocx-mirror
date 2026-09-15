"""The `OCX_EXTRA_CA_CERTS` startup gate.

The mirror resolves the operator's extra CA bundle once, before any command
dispatches, so a bundle that cannot be used fails the process there under
`ocx`'s own exit code for it — 74 for a file it could not read, 65 for a file
whose content is not a certificate bundle — with the full error chain on
stderr, never as a TLS handshake error deep inside a leg.

No registry: `package validate` is the no-network command the gate runs in
front of, and the runner is built by hand so the `registry` fixture (which
starts Docker) is never requested.
"""
from __future__ import annotations

import errno
import os
from pathlib import Path

import pytest

from src.mirror_runner import MirrorRunner

FIXTURE_SPEC = Path(__file__).resolve().parent.parent / "fixtures" / "mirror-shfmt-minimal" / "mirror.yml"


@pytest.fixture()
def gated_mirror(mirror_binary: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    """A runner whose child sees the `OCX_EXTRA_CA_CERTS` the test sets."""

    def build(value: str) -> MirrorRunner:
        monkeypatch.setenv("OCX_EXTRA_CA_CERTS", value)
        work = tmp_path / "mirror-work"
        work.mkdir(exist_ok=True)
        return MirrorRunner(mirror_binary, "127.0.0.1:1", work)

    return build


def test_the_gate_is_open_without_the_variable(
    mirror_binary: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The control: the same command exits 0 when no bundle is named."""
    monkeypatch.delenv("OCX_EXTRA_CA_CERTS", raising=False)
    runner = MirrorRunner(mirror_binary, "127.0.0.1:1", tmp_path)
    result = runner.run("package", "validate", str(FIXTURE_SPEC), check=False)
    assert result.returncode == 0, result.stderr


def test_a_bundle_file_that_is_not_a_certificate_exits_65(gated_mirror, tmp_path: Path) -> None:
    bundle = tmp_path / "not-a-bundle.pem"
    bundle.write_text("not a certificate\n")

    result = gated_mirror(str(bundle)).run("package", "validate", str(FIXTURE_SPEC), check=False)

    assert result.returncode == 65, f"rc={result.returncode}\nstderr: {result.stderr}"
    assert "OCX_EXTRA_CA_CERTS" in result.stderr, result.stderr


def test_a_bundle_path_that_does_not_exist_exits_74_with_the_reason(gated_mirror, tmp_path: Path) -> None:
    missing = tmp_path / "absent.pem"

    result = gated_mirror(str(missing)).run("package", "validate", str(FIXTURE_SPEC), check=False)

    assert result.returncode == 74, f"rc={result.returncode}\nstderr: {result.stderr}"
    assert "OCX_EXTRA_CA_CERTS" in result.stderr, result.stderr
    # The source chain: `cannot read OCX_EXTRA_CA_CERTS=<path>` alone says
    # nothing about *why*; the OS reason rides on the chain `{err:#}` renders.
    assert os.strerror(errno.ENOENT) in result.stderr, result.stderr
