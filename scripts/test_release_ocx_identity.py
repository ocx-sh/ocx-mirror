# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 The OCX Authors
"""`release_ocx_identity.py`: which signer identity a release mode may trust."""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path

import pytest

SCRIPT = Path(__file__).absolute().parent / "release_ocx_identity.py"
BASE = "https://github.com/ocx-sh/ocx/.github/workflows/release.yml@refs/heads/"
MAIN = BASE + "main"
BRANCH = BASE + "hex/signing-dogfood"


def check(mode: str, identity: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, str(SCRIPT), mode, identity], capture_output=True, text=True, check=False
    )


@pytest.mark.parametrize(
    ("mode", "identity"),
    [("dev", BRANCH), ("dev", MAIN), ("release", MAIN), ("candidate", MAIN), ("backfill", MAIN)],
)
def test_accepted(mode: str, identity: str) -> None:
    assert check(mode, identity).returncode == 0


@pytest.mark.parametrize(
    ("mode", "identity"),
    [
        ("release", BRANCH),
        ("candidate", BRANCH),
        ("backfill", BRANCH),
        ("backfill", MAIN + "-evil"),
        ("release", MAIN + "-evil"),
        ("release", MAIN + "/x"),
        ("nonsense", MAIN),
        ("", MAIN),
    ],
)
def test_refused(mode: str, identity: str) -> None:
    result = check(mode, identity)
    assert result.returncode != 0
    assert result.stdout.startswith("::error::")
    assert "ocx-identity.env" in result.stdout
